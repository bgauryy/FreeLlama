//! An Ollama-compatible, byte-preserving HTTP sidecar.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use reqwest::{Client, Url};
use tokio::sync::Mutex as AsyncMutex;

/// Total attempts (first try + retries) for a request that hits a transient upstream failure.
/// Matches the general-purpose default cited across retry-policy guidance for slow (LLM-scale,
/// not microservice-RPC-scale) calls: enough to ride out a brief hiccup, few enough that a
/// persistent outage still fails in bounded time.
const MAX_ATTEMPTS: u32 = 3;
/// Exponential backoff base; attempt `n` (1-indexed) waits `RETRY_BASE_DELAY * 2^(n-1)` plus
/// jitter, so `[200ms, 400ms]` becomes the base sequence. Jitter avoids synchronized retries
/// across concurrent callers piling back onto a recovering upstream at the same instant.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
/// Upper bound on added random jitter per retry.
const RETRY_JITTER_MAX: Duration = Duration::from_millis(100);
/// Minimum time between two automatic Ollama restart attempts. Guards against a restart storm if
/// Ollama is down for an extended period and keeps failing every request — one attempt, then a
/// long cooldown before trying again, not a loop hammering `open -a Ollama`.
const RESTART_COOLDOWN: Duration = Duration::from_secs(300);

/// What "restart Ollama" actually does. A trait object (not a plain fn pointer) so tests can
/// inject a closure that records the call instead of actually killing and relaunching a real
/// system process — see `with_restart_action`.
pub type RestartAction = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Quits and relaunches the macOS Ollama app — the same two commands
/// `benchmark/local/scripts/restart_ollama.sh` already uses, now reachable from inside the proxy
/// itself instead of only as an external script a human runs by hand. Only implemented for
/// macOS (the only platform this project runs on); everywhere else this is a documented no-op
/// rather than a silent failure.
fn default_restart_ollama() -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async {
        if !cfg!(target_os = "macos") {
            eprintln!("proxy: auto-restart is only implemented for the macOS Ollama app; skipping");
            return;
        }
        eprintln!("proxy: quitting and relaunching the Ollama app...");
        let _ = tokio::task::spawn_blocking(|| {
            std::process::Command::new("osascript")
                .args(["-e", "quit app \"Ollama\""])
                .output()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = std::process::Command::new("open").args(["-a", "Ollama"]).spawn();
    })
}

/// Cheap, dependency-free jitter source (not cryptographic — just enough to desynchronize
/// retries). Derived from the low bits of the system clock.
fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    RETRY_JITTER_MAX * (nanos % 1000) / 1000
}
/// Request bodies are buffered (not streamed) so a failed attempt can be resent byte-for-byte.
/// Ollama chat/generate payloads are JSON, not large uploads, so this bound is generous.
const MAX_BUFFERED_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Default per-attempt upstream timeout. Generous enough for a slow local-model generation turn,
/// bounded enough that a hung connection fails fast instead of blocking the caller indefinitely.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone)]
pub struct ProxyConfig {
    pub listen: String,
    pub upstream: String,
    pub allow_remote: bool,
    pub request_timeout: Duration,
    /// Opt-in: attempt one automatic Ollama restart when a request fails with a true
    /// connection-refused error (the process is gone, not just slow or erroring). Off by
    /// default — never restart a system process a caller didn't explicitly ask for.
    pub auto_restart_ollama: bool,
    restart_action: RestartAction,
}

impl ProxyConfig {
    #[must_use]
    pub fn new(listen: impl Into<String>, upstream: impl Into<String>, allow_remote: bool) -> Self {
        Self {
            listen: listen.into(),
            upstream: upstream.into(),
            allow_remote,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            auto_restart_ollama: false,
            restart_action: Arc::new(default_restart_ollama),
        }
    }

    /// Overrides the per-attempt upstream timeout (default 120s).
    #[must_use]
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Enables automatic Ollama restart on a true connection-refused failure (see
    /// `auto_restart_ollama`'s doc comment). Off by default.
    #[must_use]
    pub fn with_auto_restart_ollama(mut self, enabled: bool) -> Self {
        self.auto_restart_ollama = enabled;
        self
    }

    /// Overrides what "restart Ollama" actually does. Production code should rely on the default
    /// (the real macOS restart sequence) and only set `with_auto_restart_ollama(true)`; this
    /// exists so tests can verify the retry-then-restart-then-retry-once-more orchestration
    /// without touching a real system process.
    #[must_use]
    pub fn with_restart_action(mut self, restart_action: RestartAction) -> Self {
        self.restart_action = restart_action;
        self
    }

    /// Validate the safe-by-default proxy boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid addresses, remote exposure without an explicit opt-in,
    /// or a recursive upstream.
    pub fn validate(&self) -> Result<()> {
        let listen: SocketAddr = self.listen.parse().context("invalid --listen address")?;
        ensure!(
            self.allow_remote || listen.ip().is_loopback(),
            "non-loopback listeners require --allow-remote"
        );
        let upstream = Url::parse(&self.upstream).context("invalid --upstream URL")?;
        ensure!(
            matches!(upstream.scheme(), "http" | "https"),
            "upstream must use HTTP or HTTPS"
        );
        let same_port = upstream.port_or_known_default() == Some(listen.port());
        let same_host = upstream.host_str().is_some_and(|host| {
            host == listen.ip().to_string()
                || (listen.ip().is_loopback() && matches!(host, "localhost" | "127.0.0.1" | "::1"))
        });
        ensure!(
            !(same_host && same_port),
            "upstream points back to the proxy"
        );
        Ok(())
    }
}

#[derive(Clone)]
struct ProxyState {
    client: Client,
    upstream: String,
    auto_restart_ollama: bool,
    restart_action: RestartAction,
    last_restart_attempt: Arc<AsyncMutex<Option<Instant>>>,
}

/// Resolve an incoming Ollama path against the configured upstream.
///
/// # Errors
///
/// Returns an error if either URL is malformed.
pub fn proxy_target(upstream: &str, path_and_query: &str) -> Result<Url> {
    let mut url = Url::parse(upstream).context("invalid upstream URL")?;
    let incoming: Uri = path_and_query.parse().context("invalid incoming URI")?;
    url.set_path(incoming.path());
    url.set_query(incoming.query());
    Ok(url)
}

/// Run the optional Ollama-compatible sidecar until Ctrl-C.
///
/// # Errors
///
/// Returns an error when configuration, binding, or serving fails.
pub async fn serve(config: ProxyConfig) -> Result<()> {
    config.validate()?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind proxy at {}", config.listen))?;
    let app = app(config.clone())?;
    println!("FreeLlama proxy listening on http://{}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serve proxy")
}

/// Build the byte-preserving Ollama proxy router for composition with platform routes.
///
/// # Errors
///
/// Returns an error when the proxy boundary is unsafe or the HTTP client cannot be built.
pub fn app(config: ProxyConfig) -> Result<Router> {
    config.validate()?;
    let state = ProxyState {
        client: Client::builder()
            .timeout(config.request_timeout)
            .build()
            .context("build upstream HTTP client")?,
        upstream: config.upstream,
        auto_restart_ollama: config.auto_restart_ollama,
        restart_action: config.restart_action,
        last_restart_attempt: Arc::new(AsyncMutex::new(None)),
    };
    Ok(Router::new().fallback(any(forward)).with_state(state))
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn forward(State(state): State<ProxyState>, request: Request) -> impl IntoResponse {
    match forward_inner(&state, request).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("proxy error: {error:#}");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"error":"upstream unavailable"}"#))
                .expect("static response is valid")
        }
    }
}

/// Send the request, retrying transient failures (5xx responses, connection errors) with linear
/// backoff. Ollama occasionally returns a 500 under load-model contention; a same-request retry
/// is enough to ride that out without surfacing an error to the caller.
async fn send_with_retries(
    state: &ProxyState,
    method: &reqwest::Method,
    target: &Url,
    headers: &HeaderMap,
    body: &axum::body::Bytes,
) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome = state
            .client
            .request(method.clone(), target.clone())
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;
        let retryable_more_attempts = attempt < MAX_ATTEMPTS;
        match outcome {
            Ok(response) if response.status().is_server_error() && retryable_more_attempts => {
                eprintln!(
                    "proxy retry attempt={attempt} status={} path={}",
                    response.status(),
                    target.path()
                );
                tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1) + jitter()).await;
            }
            Ok(response) => return Ok(response),
            Err(error) if retryable_more_attempts => {
                eprintln!("proxy retry attempt={attempt} error={error:#} path={}", target.path());
                tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1) + jitter()).await;
            }
            Err(error) => return Err(error).context("forward request to Ollama"),
        }
    }
}

/// True only for a genuine "nothing is listening" failure (connection refused / DNS / TLS at the
/// transport level) — never for a slow response (that's a timeout) or a live server returning an
/// error status (that's a 5xx, already handled by `send_with_retries`). Restarting Ollama is only
/// ever the right move for the first case; the other two would just add downtime for nothing.
fn is_connection_refused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(reqwest::Error::is_connect)
}

/// Attempts one automatic Ollama restart, gated by `RESTART_COOLDOWN` so a sustained outage
/// produces one restart attempt followed by a long quiet period, not a loop. Returns whether a
/// restart was actually attempted (the caller only retries the request if it was).
async fn try_restart_ollama(state: &ProxyState) -> bool {
    let mut last_attempt = state.last_restart_attempt.lock().await;
    if let Some(previous) = *last_attempt
        && previous.elapsed() < RESTART_COOLDOWN
    {
        eprintln!(
            "proxy: Ollama connection refused, but a restart was already attempted within the \
             last {}s — not attempting another one yet",
            RESTART_COOLDOWN.as_secs()
        );
        return false;
    }
    *last_attempt = Some(Instant::now());
    drop(last_attempt);
    eprintln!("proxy: Ollama connection refused — attempting one automatic restart");
    (state.restart_action)().await;
    true
}

async fn forward_inner(state: &ProxyState, request: Request) -> Result<Response<Body>> {
    let started = Instant::now();
    let target = proxy_target(&state.upstream, request.uri().to_string().as_str())?;
    let (parts, body) = request.into_parts();
    // Buffer the body up front: a retried attempt must resend the exact same bytes, and a
    // streamed body can only be consumed once.
    let body_bytes = to_bytes(body, MAX_BUFFERED_REQUEST_BODY_BYTES)
        .await
        .context("buffer request body for retry-safe forwarding")?;
    let headers = filtered_headers(&parts.headers);

    let outcome = send_with_retries(state, &parts.method, &target, &headers, &body_bytes).await;
    let response = match outcome {
        Ok(response) => response,
        Err(error)
            if state.auto_restart_ollama
                && is_connection_refused(&error)
                && try_restart_ollama(state).await =>
        {
            // One more full attempt (with its own internal MAX_ATTEMPTS retries) after the
            // restart — not an unbounded loop back into this same branch.
            send_with_retries(state, &parts.method, &target, &headers, &body_bytes).await?
        }
        Err(error) => return Err(error),
    };
    let status = response.status();
    let headers = filtered_headers(response.headers());
    let mut outgoing = Response::builder().status(status);
    for (name, value) in &headers {
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.header("x-freellama-proxy", "1");
    let result = outgoing
        .body(Body::from_stream(response.bytes_stream()))
        .context("build proxied response")?;
    eprintln!(
        "proxy method={} path={} upstream_status={} upstream_headers_ms={}",
        parts.method,
        parts.uri,
        status,
        started.elapsed().as_millis()
    );
    Ok(result)
}

fn filtered_headers(input: &HeaderMap) -> HeaderMap {
    let mut output = HeaderMap::new();
    for (name, value) in input {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            output.append(
                HeaderName::from_bytes(name.as_str().as_bytes()).expect("HTTP header name"),
                HeaderValue::from_bytes(value.as_bytes()).expect("HTTP header value"),
            );
        }
    }
    output
}
