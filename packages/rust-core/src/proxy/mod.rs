//! An Ollama-compatible, byte-preserving HTTP sidecar.

use std::{
    io::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use reqwest::{Client, Url};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::platform::Telemetry;
use crate::platform::resources::ResourceGovernor;

mod admission;
mod config;
mod retry;

use admission::AdmittedBody;
pub use admission::{RawAdmission, RawRejection};
pub use config::ProxyConfig;
pub use retry::RestartAction;
pub(crate) use retry::{MAX_ATTEMPTS, retry_delay, retryable_upstream_status};
use retry::{is_connection_refused, send_with_retries, try_restart_ollama};

/// Request bodies are buffered (not streamed) so a failed attempt can be resent byte-for-byte.
/// Ollama chat/generate payloads are JSON, not large uploads, so this bound is generous.
const MAX_BUFFERED_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Establishing a loopback connection is instant; a long connect wait only delays the
/// connection-refused handling that restarts or reports a stopped Ollama.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a raw generation waits for a host memory/pressure hold to clear before a 503 with
/// `retry-after`. Short, because raw callers cannot be queued fairly; managed `/tasks` queue.
const RAW_PRESSURE_WAIT: Duration = Duration::from_secs(2);
/// Ollama endpoints that only move model files. They never load a runner, so they neither take
/// the execution lock nor wait on memory, so a multi-minute pull does not refuse managed tasks.
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
    // A closed stdout must not stop the server; the banner is informational.
    let _ = writeln!(
        std::io::stdout(),
        "FreeLlama proxy listening on http://{}",
        config.listen
    );
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

#[expect(
    clippy::too_many_lines,
    reason = "admission steps must stay in this order"
)]
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
