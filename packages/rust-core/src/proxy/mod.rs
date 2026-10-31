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
use crate::platform::resources::{
    ResourceDemand, ResourceGovernor, ResourcePermit, ResourceWaitError,
};

mod admission;
mod config;
mod hooks;
mod retry;
mod systemone;
mod upstreams;
mod usage;

use admission::AdmittedBody;
pub use admission::{RawAdmission, RawRejection};
pub use config::ProxyConfig;
pub use hooks::{Caller, HookFuture, RawFinished, RawModelHooks, RawUsage};
pub use retry::RestartAction;
pub(crate) use retry::{MAX_ATTEMPTS, retry_delay, retryable_upstream_status};
use retry::{is_connection_refused, send_with_retries, try_restart_ollama};
pub(crate) use upstreams::validate as validate_upstreams;
pub use upstreams::{OpenAiUpstream, load_upstreams};

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
    hooks: Option<Arc<dyn RawModelHooks>>,
    upstreams: Option<upstreams::Upstreams>,
    max_request_bytes: usize,
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
        client: crate::net::build_http_client(|builder| {
            builder
                .connect_timeout(CONNECT_TIMEOUT.min(config.request_timeout))
                .read_timeout(config.request_timeout)
        })
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
        hooks: config.hooks,
        upstreams: (!config.upstreams.is_empty())
            .then(|| upstreams::Upstreams::new(config.upstreams)),
        max_request_bytes: crate::net::max_request_bytes(),
    };
    Ok(Router::new().fallback(any(forward)).with_state(state))
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// How a raw request is admitted. Only generations take the execution lock and memory checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    if matches!(path, "/api/generate" | "/api/chat" | "/api/embed")
        && let Ok(body) = serde_json::from_slice::<serde_json::Value>(body)
        && body.get("keep_alive").is_some_and(is_zero_keep_alive)
        && body
            .get("prompt")
            .is_none_or(|value| value.as_str() == Some(""))
        && body
            .get("messages")
            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        && body.get("input").is_none_or(|value| {
            value.as_str() == Some("") || value.as_array().is_some_and(Vec::is_empty)
        })
        && body.get("images").is_none()
    {
        return RawRequestKind::Unload;
    }
    RawRequestKind::Generation
}

/// Host-pressure admission for a raw generation. Without a hold it admits at once. Under a hold, a
/// request for a model Ollama already runs waits as resident reuse, which memory-capacity holds
/// spare: the load that caused the hold must stay usable. Anything else waits as a load of
/// unknown size, so a cold load still waits for recovery. `resident` is the residency already
/// observed for this request, if any.
async fn raw_resource_permit(
    state: &ProxyState,
    model: Option<&RawModel>,
    resident: Option<bool>,
) -> Result<ResourcePermit, ResourceWaitError> {
    let governor = &state.resource_governor;
    if let Ok(permit) = governor
        .wait_for_capacity(&state.upstream, 0, Duration::ZERO)
        .await
    {
        return Ok(permit);
    }
    let resident = match (resident, model) {
        (Some(resident), _) => resident,
        (None, Some(model)) => model.clone().observe(state).await,
        (None, None) => false,
    };
    let demand = if resident {
        ResourceDemand::resident()
    } else {
        ResourceDemand::load(0)
    };
    governor
        .wait_for_demand(&state.upstream, demand, RAW_PRESSURE_WAIT)
        .await
}

/// The model a raw request names, and the context size it asks for.
#[derive(Clone)]
struct RawModel {
    name: String,
    context: Option<u64>,
}

impl RawModel {
    fn parse(body: &[u8]) -> Option<Self> {
        let body = serde_json::from_slice::<serde_json::Value>(body).ok()?;
        Some(Self {
            name: body.get("model")?.as_str()?.to_owned(),
            context: body
                .pointer("/options/num_ctx")
                .and_then(serde_json::Value::as_u64),
        })
    }

    /// Whether Ollama runs this model at a compatible context: an explicit `options.num_ctx`
    /// other than the resident `context_length` makes Ollama reload the runner. A bare name
    /// (`gemma3`) means `gemma3:latest` to Ollama, so residency, busy marks and the audit all key
    /// on that tag.
    async fn observe(&mut self, state: &ProxyState) -> bool {
        let get = |path: &'static str| {
            state
                .client
                .get(format!("{}{path}", state.upstream.trim_end_matches('/')))
                .send()
        };
        let Ok(ps) = async { get("/api/ps").await?.json::<serde_json::Value>().await }.await else {
            return false;
        };
        // Resolve exactly as Ollama will read the name it receives: the tag, else `:latest`.
        if let Some(name) = crate::ollama_http::resolve_as_ollama(
            &crate::ollama_http::listed_names(&ps),
            &self.name,
        ) {
            name.clone_into(&mut self.name);
        } else if !self.name.contains(':') {
            self.name.push_str(":latest");
        }
        crate::ollama_http::find_ps_entry(&ps, &self.name).is_some_and(|entry| {
            self.context.is_none_or(|requested| {
                entry
                    .get("context_length")
                    .and_then(serde_json::Value::as_u64)
                    == Some(requested)
            })
        })
    }
}

fn json_response(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("static response is valid")
}

fn too_large(limit: usize) -> Response<Body> {
    json_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        serde_json::json!({"error": format!(
            "request body is over {limit} bytes; raise FREELLAMA_MAX_REQUEST_BYTES to accept it"
        )})
        .to_string(),
    )
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
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let declared = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|bytes| bytes > state.max_request_bytes) {
        return too_large(state.max_request_bytes);
    }
    // Buffer the body before taking any lock: a retried attempt must resend the exact same bytes,
    // and a slow or incomplete upload must never hold the shared execution lock while it trickles.
    let body_bytes = match tokio::time::timeout(
        state.request_body_timeout,
        // Buffered (not streamed) so a failed attempt can be resent byte-for-byte.
        to_bytes(body, state.max_request_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            eprintln!("proxy error: buffer request body: {error:#}");
            // A chunked upload has no length up front; its overflow surfaces here.
            return if format!("{error:#}").contains("length limit exceeded") {
                too_large(state.max_request_bytes)
            } else {
                json_response(
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"could not read request body"}"#.into(),
                )
            };
        }
        Err(_) => {
            return json_response(
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"request body read deadline exceeded"}"#.into(),
            );
        }
    };
    let body_bytes = if parts.method == reqwest::Method::POST && parts.uri.path() == "/v1/systemone"
    {
        match systemone::assets_as_images(&body_bytes) {
            Ok(Some(translated)) => translated,
            Ok(None) => body_bytes,
            Err(message) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error": message}).to_string(),
                );
            }
        }
    } else {
        body_bytes
    };
    if let Some(response) = route_to_upstream(&state, &parts, &body_bytes, started).await {
        return response;
    }
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
    let mut model = matches!(kind, RawRequestKind::Generation | RawRequestKind::Unload)
        .then(|| RawModel::parse(&body_bytes))
        .flatten();
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
    // A raw generation for a model Ollama already holds at a compatible context shares the
    // backend with managed tasks on resident runners; only a load takes the transition lock
    // exclusively, as a managed load does. Residency is checked under the shared side, so no
    // managed transition can unload the runner between the check and the request.
    let mut observed = None;
    let execution = if generation && let Some(lock) = state.execution_lock.as_ref() {
        let shared = tokio::time::timeout_at(deadline, Arc::clone(lock).read_owned()).await;
        let resident_now = match (&shared, &state.hooks, &mut model) {
            (Ok(_), Some(_), Some(model)) => Some(model.observe(&state).await),
            _ => None,
        };
        let guard = match (shared, resident_now) {
            (Ok(shared), Some(true)) => {
                observed = Some(true);
                Ok(admission::ExecutionGuard::Shared(shared))
            }
            (Ok(shared), _) => {
                drop(shared);
                tokio::time::timeout_at(deadline, Arc::clone(lock).write_owned())
                    .await
                    .map(admission::ExecutionGuard::Exclusive)
            }
            (Err(elapsed), _) => Err(elapsed),
        };
        if let Ok(guard) = guard {
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
    // With platform hooks, residency decides whether this request loads a model: a load makes
    // room first, as a managed load would, and the model counts as busy until the response ends.
    let resident = match (&state.hooks, &mut model) {
        (Some(hooks), Some(model)) => {
            let resident = match observed {
                Some(resident) => resident,
                None => model.observe(&state).await,
            };
            if generation && !resident {
                hooks.prepare(&model.name, model.context).await;
            }
            Some(resident)
        }
        _ => None,
    };
    let in_use = state
        .hooks
        .as_ref()
        .zip(model.as_ref().filter(|_| generation))
        .map(|(hooks, model)| hooks.begin(&model.name));
    let resource = if generation {
        match raw_resource_permit(&state, model.as_ref(), resident).await {
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
            let mut usage = None;
            if response.status().is_success()
                && let (Some(hooks), Some(model)) = (&state.hooks, &model)
            {
                if generation {
                    let loaded = resident == Some(false);
                    hooks.served(&model.name, loaded);
                    usage = Some(usage_tap(
                        hooks,
                        "gpu",
                        &model.name,
                        &parts,
                        loaded,
                        started,
                    ));
                } else if resident == Some(true) {
                    // Only a runner that was loaded is unloaded; anything else changed nothing.
                    hooks.unloaded(&model.name);
                }
            }
            let (response_parts, body) = response.into_parts();
            Response::from_parts(
                response_parts,
                Body::new(AdmittedBody {
                    inner: body,
                    permit,
                    execution,
                    resource,
                    in_use,
                    usage,
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

/// Read what a response reports using and hand it to the platform once it ends.
fn usage_tap(
    hooks: &Arc<dyn RawModelHooks>,
    backend: &str,
    model: &str,
    parts: &axum::http::request::Parts,
    loaded: bool,
    started: Instant,
) -> admission::UsageTap {
    let hooks = Arc::clone(hooks);
    let (backend, model, path) = (
        backend.to_owned(),
        model.to_owned(),
        parts.uri.path().to_owned(),
    );
    let caller = parts
        .extensions
        .get::<Caller>()
        .map(|caller| Arc::clone(&caller.0));
    admission::UsageTap {
        scanner: usage::UsageScanner::default(),
        finished: Some(Box::new(move |usage| {
            hooks.finished(RawFinished {
                backend: &backend,
                model: &model,
                path: &path,
                caller: caller.as_deref(),
                loaded,
                usage,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        })),
    }
}

/// An OpenAI-compatible request for a model another engine serves, answered by that engine.
/// `None` sends the request on to Ollama as before.
async fn route_to_upstream(
    state: &ProxyState,
    parts: &axum::http::request::Parts,
    body: &Bytes,
    started: Instant,
) -> Option<Response<Body>> {
    let upstreams = state.upstreams.as_ref()?;
    let path = parts.uri.path();
    if !path.starts_with("/v1/") {
        return None;
    }
    if parts.method == reqwest::Method::GET && path == "/v1/models" {
        let merged = upstreams
            .merged_models(&state.client, &state.upstream)
            .await;
        return Some(json_response(StatusCode::OK, merged.to_string()));
    }
    if parts.method != reqwest::Method::POST {
        return None;
    }
    let mut request = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let model = request.get("model")?.as_str()?.to_owned();
    let upstreams::Route::Upstream(index, name) = upstreams
        .route(&state.client, &state.upstream, &model)
        .await
    else {
        return None;
    };
    let body = if name == model {
        body.clone()
    } else {
        request["model"] = serde_json::json!(name);
        Bytes::from(request.to_string())
    };
    Some(forward_to_upstream(state, upstreams, index, parts, body, &model, started).await)
}

async fn forward_to_upstream(
    state: &ProxyState,
    upstreams: &upstreams::Upstreams,
    index: usize,
    parts: &axum::http::request::Parts,
    body: Bytes,
    model: &str,
    started: Instant,
) -> Response<Body> {
    let upstream = &upstreams.list[index];
    // An engine on this machine shares its memory and heat: wait out a hold as a resident model
    // would. A remote one does not.
    let resource = if crate::net::is_loopback_url(&upstream.base_url) {
        match state
            .resource_governor
            .wait_for_demand(
                &upstream.base_url,
                ResourceDemand::resident(),
                RAW_PRESSURE_WAIT,
            )
            .await
        {
            Ok(permit) => Some(permit),
            Err(error) => return refusal(StatusCode::SERVICE_UNAVAILABLE, &error.to_string(), 2),
        }
    } else {
        None
    };
    let mut headers = filtered_headers(&parts.headers);
    // The caller's bearer token is for FreeLlama; the upstream gets its own key or none.
    headers.remove(axum::http::header::AUTHORIZATION);
    let mut outgoing = state
        .client
        .post(upstreams.url(index, &parts.uri.to_string()))
        .headers(headers)
        .body(body);
    if let Some(key) = &upstream.api_key {
        outgoing = outgoing.bearer_auth(key);
    }
    let count = |outcome: &str| {
        if let Some(telemetry) = &state.telemetry {
            telemetry.record_raw("upstream", outcome);
        }
    };
    let response = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            count("upstream_error");
            eprintln!("proxy error: upstream {}: {error:#}", upstream.name);
            return json_response(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": format!("upstream {} unavailable", upstream.name)})
                    .to_string(),
            );
        }
    };
    count("forwarded");
    let status = response.status();
    let mut builder = Response::builder().status(status);
    for (header, value) in &filtered_headers(response.headers()) {
        builder = builder.header(header, value);
    }
    let usage = state
        .hooks
        .as_ref()
        .filter(|_| status.is_success())
        .map(|hooks| usage_tap(hooks, &upstream.name, model, parts, false, started));
    builder
        .header("x-freellama-proxy", "1")
        .header("x-freellama-upstream", upstream.name.as_str())
        .body(Body::new(AdmittedBody {
            inner: Body::from_stream(response.bytes_stream()),
            permit: None,
            execution: None,
            resource,
            in_use: None,
            usage,
        }))
        .unwrap_or_else(|_| {
            json_response(
                StatusCode::BAD_GATEWAY,
                r#"{"error":"upstream response headers are invalid"}"#.into(),
            )
        })
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

#[cfg(test)]
mod tests {
    use super::{RawRequestKind, classify};
    use reqwest::Method;

    #[test]
    fn an_empty_request_with_zero_keep_alive_is_an_unload_on_every_load_path() {
        for (path, body) in [
            ("/api/generate", r#"{"model":"m","keep_alive":0}"#),
            (
                "/api/chat",
                r#"{"model":"m","messages":[],"keep_alive":"0"}"#,
            ),
            ("/api/embed", r#"{"model":"m","input":[],"keep_alive":0}"#),
        ] {
            assert_eq!(
                classify(&Method::POST, path, body.as_bytes()),
                RawRequestKind::Unload,
                "{path}"
            );
        }
        for (path, body) in [
            (
                "/api/embed",
                r#"{"model":"m","input":["hi"],"keep_alive":0}"#,
            ),
            (
                "/api/embed",
                r#"{"model":"m","input":[],"keep_alive":"5m"}"#,
            ),
            (
                "/api/generate",
                r#"{"model":"m","prompt":"hi","keep_alive":0}"#,
            ),
        ] {
            assert_eq!(
                classify(&Method::POST, path, body.as_bytes()),
                RawRequestKind::Generation,
                "{body}"
            );
        }
    }
}
