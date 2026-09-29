//! An Ollama-compatible, byte-preserving HTTP sidecar.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use crate::platform::Telemetry;
use crate::platform::resources::{ResourceGovernor, ResourcePermit};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use reqwest::{Client, Url};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{Notify, OwnedRwLockWriteGuard, RwLock};

/// Total attempts (first try + retries) for a request that hits a transient upstream failure.
/// Matches the general-purpose default cited across retry-policy guidance for slow (LLM-scale,
/// not microservice-RPC-scale) calls: enough to ride out a brief hiccup, few enough that a
/// persistent outage still fails in bounded time.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Exponential backoff base; attempt `n` (1-indexed) waits `RETRY_BASE_DELAY * 2^(n-1)` plus
/// jitter, so `[200ms, 400ms]` becomes the base sequence. Jitter avoids synchronized retries
/// across concurrent callers piling back onto a recovering upstream at the same instant.
pub(crate) const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
/// Upper bound on added random jitter per retry.
pub(crate) const RETRY_JITTER_MAX: Duration = Duration::from_millis(100);
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
        let _ = std::process::Command::new("open")
            .args(["-a", "Ollama"])
            .spawn();
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

/// Backoff before retrying `attempt` (1-indexed): `RETRY_BASE_DELAY * 2^(attempt-1)` plus jitter.
///
/// Shared with the managed-task path in `platform/mod.rs` so both retry-capable callers use one
/// backoff policy rather than drifting apart — the passthrough and the managed plane hit the same
/// Ollama, and a divergent schedule on one of them is a bug nobody would notice until it hurt.
pub(crate) fn retry_delay(attempt: u32) -> Duration {
    RETRY_BASE_DELAY * 2u32.pow(attempt.saturating_sub(1)) + jitter()
}
/// Request bodies are buffered (not streamed) so a failed attempt can be resent byte-for-byte.
/// Ollama chat/generate payloads are JSON, not large uploads, so this bound is generous.
const MAX_BUFFERED_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Default upstream idle timeout: the longest gap allowed between bytes from Ollama (including
/// the wait for response headers while a model loads). It is not a total deadline, so a streamed
/// generation that keeps producing tokens is never cut off mid-answer, while a wedged connection
/// still fails instead of blocking the caller indefinitely.
/// Matches Ollama's own `OLLAMA_LOAD_TIMEOUT` (5m), so a cold load of a large model is not cut
/// off by the proxy before Ollama itself would give up.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Establishing a loopback connection is instant; a long connect wait only delays the
/// connection-refused handling that restarts or reports a stopped Ollama.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a raw generation waits for a host memory/pressure hold to clear before a 503 with
/// `retry-after`. Short, because raw callers cannot be queued fairly; managed `/tasks` queue.
const RAW_PRESSURE_WAIT: Duration = Duration::from_secs(2);
/// Ollama endpoints that only move model files. They never load a runner, so they neither take
/// the execution lock nor wait on memory: a multi-minute pull used to hold the exclusive lock and
/// refuse every managed task until the download finished.
const MODEL_STORE_PATHS: &[&str] = &["/api/pull", "/api/push", "/api/copy", "/api/delete"];

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
    resource_governor: ResourceGovernor,
    pub listen: String,
    pub upstream: String,
    pub allow_remote: bool,
    pub request_timeout: Duration,
    /// Opt-in: attempt one automatic Ollama restart when a request fails with a true
    /// connection-refused error (the process is gone, not just slow or erroring). Off by
    /// default — never restart a system process a caller didn't explicitly ask for.
    pub auto_restart_ollama: bool,
    /// Optional byte-preserving proxy inflight cap. Unlike managed admission this cannot infer
    /// task cost or CPU/GPU placement, so it is intentionally an immediate generic refusal.
    pub max_concurrent_requests: Option<usize>,
    /// How long a raw generation may wait for a free slot and for managed execution to finish
    /// before being refused. Zero (the standalone default) refuses immediately.
    pub raw_queue_wait: Duration,
    execution_lock: Option<Arc<RwLock<()>>>,
    restart_action: RestartAction,
    raw_admission: Option<RawAdmission>,
    telemetry: Option<Telemetry>,
}

impl ProxyConfig {
    #[must_use]
    pub fn new(listen: impl Into<String>, upstream: impl Into<String>, allow_remote: bool) -> Self {
        Self {
            resource_governor: ResourceGovernor::default(),
            listen: listen.into(),
            upstream: upstream.into(),
            allow_remote,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            auto_restart_ollama: false,
            max_concurrent_requests: None,
            raw_queue_wait: Duration::ZERO,
            execution_lock: None,
            restart_action: Arc::new(default_restart_ollama),
            raw_admission: None,
            telemetry: None,
        }
    }

    /// Overrides the upstream idle timeout (default 300s): the longest silence allowed between
    /// bytes, not a total deadline. It also bounds reading the incoming request body.
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

    /// Bound simultaneous raw compatibility requests. Off by default to preserve existing proxy
    /// semantics; managed `/tasks` should be preferred for weighted CPU/GPU admission.
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, max: usize) -> Self {
        self.max_concurrent_requests = Some(max.max(1));
        self
    }

    /// Wait up to `wait` for a raw slot (and for managed execution) before refusing.
    #[must_use]
    pub fn with_raw_queue_wait(mut self, wait: Duration) -> Self {
        self.raw_queue_wait = wait;
        self
    }

    /// Use a shared, live-adjustable raw admission instead of a fixed cap. The platform passes
    /// one so a runtime-config reload can change the limit and wait without a restart.
    #[must_use]
    pub fn with_raw_admission(mut self, admission: RawAdmission) -> Self {
        self.raw_admission = Some(admission);
        self
    }

    /// Count raw requests and refusals in the platform's metrics.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Telemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Share host pressure and memory reservations with managed execution.
    #[must_use]
    pub fn with_resource_governor(mut self, governor: ResourceGovernor) -> Self {
        self.resource_governor = governor;
        self
    }

    /// Share a backend's managed execution boundary. Raw mutations may load or unload any model,
    /// so they require exclusive access until their response stream ends. Metadata GET/HEAD
    /// requests remain available while managed execution is active.
    #[must_use]
    pub fn with_execution_lock(mut self, execution_lock: Arc<RwLock<()>>) -> Self {
        self.execution_lock = Some(execution_lock);
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
            // `url` renders IPv6 hosts with brackets while `SocketAddr::ip()` does not. Compare
            // their canonical bracket-free forms so `[::1]` cannot evade recursive-upstream
            // detection.
            let host = host
                .strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host);
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
    resource_governor: ResourceGovernor,
    client: Client,
    request_body_timeout: Duration,
    upstream: String,
    auto_restart_ollama: bool,
    restart_action: RestartAction,
    last_restart_attempt: Arc<AsyncMutex<Option<Instant>>>,
    admission: Option<RawAdmission>,
    raw_queue_wait: Duration,
    execution_lock: Option<Arc<RwLock<()>>>,
    telemetry: Option<Telemetry>,
}

/// Raw passthrough admission: a concurrency limit with a bounded wait, adjustable at runtime.
///
/// Raw callers cannot declare a task cost, so this is a plain count, not weighted admission.
/// Waiters are woken together and race for a freed slot: bounded, not strictly FIFO.
#[derive(Clone)]
pub struct RawAdmission {
    inner: Arc<RawAdmissionInner>,
}

struct RawAdmissionInner {
    state: std::sync::Mutex<RawAdmissionState>,
    changed: Notify,
}

#[derive(Debug)]
struct RawAdmissionState {
    active: usize,
    limit: usize,
    wait: Duration,
    waiting: usize,
    admitted: u64,
    rejected: u64,
    /// Exponentially weighted service time of finished raw requests, for `Retry-After`.
    average_ms: Option<u64>,
}

/// Why a raw request was refused, with a `Retry-After` estimate in seconds.
pub struct RawRejection {
    pub reason: &'static str,
    pub retry_after_seconds: u64,
}

impl RawAdmission {
    #[must_use]
    pub fn new(limit: usize, wait: Duration) -> Self {
        Self {
            inner: Arc::new(RawAdmissionInner {
                state: std::sync::Mutex::new(RawAdmissionState {
                    active: 0,
                    limit: limit.max(1),
                    wait,
                    waiting: 0,
                    admitted: 0,
                    rejected: 0,
                    average_ms: None,
                }),
                changed: Notify::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RawAdmissionState> {
        self.inner.state.lock().expect("raw admission poisoned")
    }

    /// Change the limit and wait; takes effect for the next request.
    pub fn set(&self, limit: usize, wait: Duration) {
        let mut state = self.lock();
        state.limit = limit.max(1);
        state.wait = wait;
        drop(state);
        self.inner.changed.notify_waiters();
    }

    #[must_use]
    pub fn wait(&self) -> Duration {
        self.lock().wait
    }

    #[must_use]
    pub fn receipt(&self) -> serde_json::Value {
        let state = self.lock();
        serde_json::json!({
            "limit": state.limit,
            "active": state.active,
            "waiting": state.waiting,
            "queue_wait_seconds": state.wait.as_secs(),
            "admitted": state.admitted,
            "rejected": state.rejected,
        })
    }

    fn retry_after(state: &RawAdmissionState) -> u64 {
        let per_request = state.average_ms.unwrap_or(5_000).max(250);
        let ahead = u64::try_from(state.waiting.saturating_add(1)).unwrap_or(u64::MAX);
        let lanes = u64::try_from(state.limit.max(1)).unwrap_or(1);
        (ahead.saturating_mul(per_request) / lanes)
            .div_ceil(1000)
            .clamp(1, 120)
    }

    /// Queue for a slot until `deadline`. The waiting count is bounded at eight per slot.
    async fn acquire(&self, deadline: tokio::time::Instant) -> Result<RawPermit, RawRejection> {
        {
            let mut state = self.lock();
            if state.active < state.limit {
                state.active += 1;
                state.admitted += 1;
                return Ok(RawPermit::new(self.clone()));
            }
            if tokio::time::Instant::now() >= deadline || state.waiting >= state.limit * 8 {
                state.rejected += 1;
                return Err(RawRejection {
                    reason: "raw request limit reached",
                    retry_after_seconds: Self::retry_after(&state),
                });
            }
            state.waiting += 1;
        }
        let result = loop {
            let changed = self.inner.changed.notified();
            {
                let mut state = self.lock();
                if state.active < state.limit {
                    state.active += 1;
                    state.admitted += 1;
                    break Ok(RawPermit::new(self.clone()));
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut state = self.lock();
                state.rejected += 1;
                break Err(RawRejection {
                    reason: "raw request limit reached",
                    retry_after_seconds: Self::retry_after(&state),
                });
            }
        };
        self.lock().waiting -= 1;
        result
    }
}

/// Held raw slot; released (and the service-time average updated) when the stream ends.
pub(crate) struct RawPermit {
    admission: RawAdmission,
    started: Instant,
}

impl RawPermit {
    fn new(admission: RawAdmission) -> Self {
        Self {
            admission,
            started: Instant::now(),
        }
    }
}

impl Drop for RawPermit {
    fn drop(&mut self) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut state = self.admission.lock();
        state.active = state.active.saturating_sub(1);
        state.average_ms = Some(state.average_ms.map_or(elapsed, |average| {
            average.saturating_mul(4).saturating_add(elapsed) / 5
        }));
        drop(state);
        self.admission.inner.changed.notify_waiters();
    }
}

/// Response body that owns the raw-admission permit for the complete upstream stream lifetime.
/// Receiving headers is not completion for Ollama: generation usually continues while bytes are
/// streamed, so releasing earlier turns a concurrency cap into a headers-only cap.
struct AdmittedBody {
    inner: Body,
    permit: Option<RawPermit>,
    execution: Option<OwnedRwLockWriteGuard<()>>,
    resource: Option<ResourcePermit>,
}

impl HttpBody for AdmittedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(context);
        if matches!(&result, Poll::Ready(None)) {
            self.permit.take();
            self.execution.take();
            self.resource.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
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
        resource_governor: config.resource_governor,
        client: Client::builder()
            .connect_timeout(CONNECT_TIMEOUT.min(config.request_timeout))
            .read_timeout(config.request_timeout)
            .build()
            .context("build upstream HTTP client")?,
        request_body_timeout: config.request_timeout,
        upstream: config.upstream,
        auto_restart_ollama: config.auto_restart_ollama,
        restart_action: config.restart_action,
        last_restart_attempt: Arc::new(AsyncMutex::new(None)),
        admission: config.raw_admission.or_else(|| {
            config
                .max_concurrent_requests
                .map(|max| RawAdmission::new(max, config.raw_queue_wait))
        }),
        raw_queue_wait: config.raw_queue_wait,
        execution_lock: config.execution_lock,
        telemetry: config.telemetry,
    };
    Ok(Router::new().fallback(any(forward)).with_state(state))
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// How a raw request is admitted. Only generations take the execution lock and memory checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RawRequestKind {
    /// Reads (GET/HEAD, `/api/show`): always available, including to diagnose pressure.
    Metadata,
    /// Pull/push/copy/delete and blob uploads: file work that never loads a runner.
    ModelStore,
    /// A `keep_alive: 0` request with nothing to generate: frees memory, so never held for it.
    Unload,
    /// Anything that may load a model or generate.
    Generation,
}

fn is_zero_keep_alive(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(number) => number.as_f64() == Some(0.0),
        serde_json::Value::String(text) => {
            let digits = text.trim().trim_end_matches(['s', 'm', 'h']);
            !digits.is_empty() && digits.parse::<f64>().is_ok_and(|value| value == 0.0)
        }
        _ => false,
    }
}

fn classify(method: &reqwest::Method, path: &str, body: &[u8]) -> RawRequestKind {
    if path.starts_with("/api/blobs/") || MODEL_STORE_PATHS.contains(&path) {
        return RawRequestKind::ModelStore;
    }
    if matches!(*method, reqwest::Method::GET | reqwest::Method::HEAD) || path == "/api/show" {
        return RawRequestKind::Metadata;
    }
    if matches!(path, "/api/generate" | "/api/chat")
        && let Ok(body) = serde_json::from_slice::<serde_json::Value>(body)
        && body.get("keep_alive").is_some_and(is_zero_keep_alive)
        && body
            .get("prompt")
            .is_none_or(|value| value.as_str() == Some(""))
        && body
            .get("messages")
            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        && body.get("images").is_none()
    {
        return RawRequestKind::Unload;
    }
    RawRequestKind::Generation
}

fn json_response(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("static response is valid")
}

fn refusal(status: StatusCode, message: &str, retry_after_seconds: u64) -> Response<Body> {
    let mut response = json_response(
        status,
        serde_json::json!({"error": message, "retry_after_seconds": retry_after_seconds})
            .to_string(),
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after_seconds.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

#[allow(clippy::too_many_lines)] // Admission steps must stay in this order; see comments.
async fn forward(State(state): State<ProxyState>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    // Buffer the body before taking any lock: a retried attempt must resend the exact same bytes,
    // and a slow or incomplete upload must never hold the shared execution lock while it trickles.
    let body_bytes = match tokio::time::timeout(
        state.request_body_timeout,
        to_bytes(body, MAX_BUFFERED_REQUEST_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            eprintln!("proxy error: buffer request body: {error:#}");
            return json_response(
                StatusCode::BAD_REQUEST,
                r#"{"error":"could not read request body"}"#.into(),
            );
        }
        Err(_) => {
            return json_response(
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"request body read deadline exceeded"}"#.into(),
            );
        }
    };
    let kind = classify(&parts.method, parts.uri.path(), &body_bytes);
    let kind_label = match kind {
        RawRequestKind::Metadata => "metadata",
        RawRequestKind::ModelStore => "model_store",
        RawRequestKind::Unload => "unload",
        RawRequestKind::Generation => "generation",
    };
    let count = |outcome: &str| {
        if let Some(telemetry) = &state.telemetry {
            telemetry.record_raw(kind_label, outcome);
        }
    };
    let generation = kind == RawRequestKind::Generation;
    let wait = state
        .admission
        .as_ref()
        .map_or(state.raw_queue_wait, RawAdmission::wait);
    let deadline = tokio::time::Instant::now() + wait;
    // Slot first, then the execution lock, so a queued raw request never holds the lock while
    // waiting for a slot.
    let permit = match state.admission.as_ref().filter(|_| generation) {
        Some(admission) => match admission.acquire(deadline).await {
            Ok(permit) => Some(permit),
            Err(rejection) => {
                count("rejected_limit");
                return refusal(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "proxy busy: {}; use managed /_freellama/v1/tasks for weighted admission",
                        rejection.reason
                    ),
                    rejection.retry_after_seconds,
                );
            }
        },
        None => None,
    };
    let execution = if generation && let Some(lock) = state.execution_lock.as_ref() {
        if let Ok(guard) = tokio::time::timeout_at(deadline, Arc::clone(lock).write_owned()).await {
            Some(guard)
        } else {
            count("rejected_busy");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy busy: backend execution active; use managed /_freellama/v1/tasks to queue",
                2,
            );
        }
    } else {
        None
    };
    let resource = if generation {
        match state
            .resource_governor
            .wait_for_capacity(&state.upstream, 0, RAW_PRESSURE_WAIT)
            .await
        {
            Ok(permit) => Some(permit),
            Err(error) => {
                count("rejected_pressure");
                let mut refusal = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({"error": error.to_string(), "resource_admission": error.receipt})
                        .to_string(),
                );
                refusal
                    .headers_mut()
                    .insert("retry-after", axum::http::HeaderValue::from_static("2"));
                return refusal;
            }
        }
    } else {
        None
    };
    match forward_inner(&state, &parts, &body_bytes).await {
        Ok(response) => {
            count("forwarded");
            let (parts, body) = response.into_parts();
            Response::from_parts(
                parts,
                Body::new(AdmittedBody {
                    inner: body,
                    permit,
                    execution,
                    resource,
                }),
            )
        }
        Err(error) => {
            count("upstream_error");
            drop((permit, execution, resource));
            eprintln!("proxy error: {error:#}");
            json_response(
                StatusCode::BAD_GATEWAY,
                r#"{"error":"upstream unavailable"}"#.into(),
            )
        }
    }
}

/// Retry 500/502 (load-model blips) and connection-establishment failures, but not 503 busy,
/// 504/client timeouts, or ambiguous failures after the upstream accepted a request.
/// A timed-out generation may still be consuming the upstream runner, so replaying it can multiply
/// load precisely when the host is under pressure. Shared with managed `/tasks` so a
/// request through the proxy cannot amplify saturation the admission semaphore is shedding.
pub(crate) fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR | StatusCode::BAD_GATEWAY
    )
}

/// Send the request, retrying transient 500/502 responses and connection-establishment errors
/// with exponential backoff. Ollama occasionally returns a 500 under load-model contention; a
/// same-request retry is enough to ride that out without surfacing an error to the caller. HTTP
/// 503, 504, client timeouts, and post-send transport failures are returned as-is.
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
            Ok(response)
                if retryable_upstream_status(response.status()) && retryable_more_attempts =>
            {
                eprintln!(
                    "proxy retry attempt={attempt} status={} path={}",
                    response.status(),
                    target.path()
                );
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Ok(response) => return Ok(response),
            // Once the upstream accepts a request, a timeout or disconnected response may leave
            // generation running. Only failures while establishing the connection are safe to
            // replay; other transport failures would risk duplicating the accepted work.
            Err(error) if retryable_more_attempts && error.is_connect() && !error.is_timeout() => {
                eprintln!(
                    "proxy retry attempt={attempt} error={error:#} path={}",
                    target.path()
                );
                tokio::time::sleep(retry_delay(attempt)).await;
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

async fn forward_inner(
    state: &ProxyState,
    parts: &axum::http::request::Parts,
    body_bytes: &Bytes,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let target = proxy_target(&state.upstream, parts.uri.to_string().as_str())?;
    let headers = filtered_headers(&parts.headers);
    let outcome = send_with_retries(state, &parts.method, &target, &headers, body_bytes).await;
    let response = match outcome {
        Ok(response) => response,
        Err(error)
            if state.auto_restart_ollama
                && is_connection_refused(&error)
                && try_restart_ollama(state).await =>
        {
            // One more full attempt (with its own internal MAX_ATTEMPTS retries) after the
            // restart — not an unbounded loop back into this same branch.
            send_with_retries(state, &parts.method, &target, &headers, body_bytes).await?
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
            // Clone, don't round-trip through bytes. The input is a `HeaderMap`, so every name and
            // value here was already validated when it was parsed; re-parsing them only added two
            // `expect()` calls — the sole panic sites in the proxy — to re-derive what the type
            // system already guarantees, on every proxied request in both directions.
            output.append(name.clone(), value.clone());
        }
    }
    output
}
