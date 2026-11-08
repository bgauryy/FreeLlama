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
use http_body::Body as _;
use reqwest::{Client, Url};
use tokio::sync::Mutex as AsyncMutex;

use crate::platform::resources::{
    ResourceDemand, ResourceGovernor, ResourcePermit, ResourceWaitError,
};

use crate::platform::{Backend, PlacementRule, Telemetry};

mod admission;
mod body_budget;
mod config;
mod hooks;
mod retry;
mod systemone;
mod upstreams;
mod usage;

use admission::AdmittedBody;
pub use admission::{RawAdmission, RawRejection};
pub(crate) use body_budget::{BodyBudget, expected_body_bytes};
pub use config::ProxyConfig;
pub use hooks::{
    Caller, ForecastFuture, HookFuture, LeaseFuture, RawFinished, RawForecast, RawModelHooks,
    RawOutcome, RawUsage,
};
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
const RAW_FORECAST_WAIT: Duration = Duration::from_secs(10);
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
    /// Control work cannot occupy generation slots while waiting for queued model users.
    unload_admission: RawAdmission,
    raw_queue_wait: Duration,
    load_lock: Option<Arc<AsyncMutex<()>>>,
    telemetry: Option<Telemetry>,
    hooks: Option<Arc<dyn RawModelHooks>>,
    upstreams: Option<upstreams::Upstreams>,
    max_request_bytes: usize,
    /// Shared with every server in the process: bytes of buffered request bodies.
    body_budget: BodyBudget,
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
        unload_admission: RawAdmission::new(1, config.raw_queue_wait),
        raw_queue_wait: config.raw_queue_wait,
        load_lock: config.load_lock,
        telemetry: config.telemetry,
        hooks: config.hooks,
        upstreams: (!config.upstreams.is_empty())
            .then(|| upstreams::Upstreams::new(config.upstreams)),
        max_request_bytes: config
            .max_request_bytes
            .unwrap_or_else(crate::net::max_request_bytes),
        body_budget: config.body_budget.unwrap_or_else(BodyBudget::shared),
    };
    Ok(Router::new().fallback(any(forward)).with_state(state))
}

async fn shutdown() {
    crate::net::shutdown_signal().await;
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

/// Forecast cold allocations even when host telemetry is healthy. Resident reuse needs the
/// compatibility observation already made under the transition guard.
async fn raw_resource_permit(
    state: &ProxyState,
    mut model: Option<&mut RawModel>,
    resident: Option<bool>,
) -> Result<ResourcePermit, ResourceWaitError> {
    let governor = &state.resource_governor;
    if model.as_deref().is_some_and(|model| model.not_installed) {
        // Nothing can load, so this claims no memory; Ollama answers with its own "not found".
        return governor
            .wait_for_demand(
                &state.upstream,
                ResourceDemand::resident(),
                RAW_PRESSURE_WAIT,
            )
            .await;
    }
    if !crate::net::is_loopback_url(&state.upstream) {
        // Local host telemetry cannot reserve memory on a remote Ollama process.
        return governor
            .wait_for_demand(&state.upstream, ResourceDemand::load(0), RAW_PRESSURE_WAIT)
            .await;
    }
    let resident = match (resident, model.as_deref_mut()) {
        (Some(resident), _) => resident,
        (None, Some(model)) => model.clone().observe(state).await,
        (None, None) => false,
    };
    // A cold load declares its installed size, so a small model (a speech or embedding model)
    // keeps the small-load exemption under swap instead of waiting as a load of unknown size.
    // No resolved model reaches here only for well-formed JSON that names none (the caller
    // refuses an unresolvable body first); Ollama rejects it before scheduling, so it can
    // allocate nothing and claims no memory.
    let demand = match model {
        Some(model) if !resident => {
            ResourceDemand::load(model.forecast_bytes.filter(|bytes| *bytes > 0).ok_or_else(
                || ResourceWaitError {
                    receipt: Box::new(crate::platform::resources::ResourceReceipt {
                        status: "model_forecast_unavailable",
                        waited_ms: 0,
                        required_available_bytes: 0,
                        reserved_bytes: 0,
                        snapshot: None,
                        assessment: None,
                    }),
                },
            )?)
        }
        _ => ResourceDemand::resident(),
    };
    governor
        .wait_for_demand(&state.upstream, demand, RAW_PRESSURE_WAIT)
        .await
}

/// Standalone fallback: installed identity plus file/overhead estimate, without `/api/show`.
async fn installed_forecast(state: &ProxyState, model: &str) -> Option<RawForecast> {
    let response = state
        .client
        .get(format!("{}/api/tags", state.upstream.trim_end_matches('/')))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let tags = response.json::<serde_json::Value>().await.ok()?;
    let entry = crate::ollama_http::find_ps_entry(&tags, model)?;
    let bytes = entry["size"].as_u64().filter(|bytes| *bytes > 0)?;
    Some(RawForecast {
        required_host_bytes: bytes.saturating_add(bytes / 3),
        context_tokens: None,
        digest: entry["digest"]
            .as_str()
            .filter(|digest| !digest.is_empty())
            .map(str::to_owned),
    })
}

/// Whether Ollama's catalog was readable and does not list `model`, as Ollama resolves the name.
async fn not_listed(state: &ProxyState, model: &str) -> bool {
    let tags = async {
        let response = state
            .client
            .get(format!("{}/api/tags", state.upstream.trim_end_matches('/')))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        response.json::<serde_json::Value>().await.ok()
    }
    .await;
    tags.is_some_and(|tags| {
        crate::ollama_http::resolve_as_ollama(&crate::ollama_http::listed_names(&tags), model)
            .is_none()
    })
}

/// The model a raw request names, and the context size it asks for.
#[derive(Clone)]
struct RawModel {
    name: String,
    context: Option<u64>,
    options_unverified: bool,
    digest: Option<String>,
    forecast_bytes: Option<u64>,
    /// Ollama's catalog was read and does not list this model: nothing can load, and Ollama's own
    /// 404 is the answer. Unknown metadata is different, and stays a refusal.
    not_installed: bool,
}

impl RawModel {
    fn parse(body: &[u8]) -> Option<Self> {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Self::from_multipart(body);
        };
        let mut name = body.get("model")?.as_str()?.to_owned();
        if name.trim().is_empty() {
            return None;
        }
        if !name.rsplit('/').next().unwrap_or(&name).contains(':') {
            name.push_str(":latest");
        }
        Some(Self {
            name,
            context: body
                .pointer("/options/num_ctx")
                .and_then(serde_json::Value::as_u64),
            options_unverified: [
                "num_batch",
                "num_thread",
                "num_gpu",
                "main_gpu",
                "numa",
                "low_vram",
                "f16_kv",
                "logits_all",
                "vocab_only",
                "use_mmap",
                "use_mlock",
                "embedding_only",
            ]
            .iter()
            .any(|key| body["options"].get(*key).is_some()),
            digest: None,
            forecast_bytes: None,
            not_installed: false,
        })
    }

    /// The `model` field of a `multipart/form-data` body (`/v1/audio/transcriptions`), whose
    /// value follows the part's blank line and ends at the next CRLF.
    fn from_multipart(body: &[u8]) -> Option<Self> {
        const FIELD: &[u8] = b"name=\"model\"";
        let at = body
            .windows(FIELD.len())
            .position(|window| window == FIELD)?
            + FIELD.len();
        let rest = &body[at..];
        let start = rest.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
        let value = &rest[start..];
        let end = value.windows(2).position(|window| window == b"\r\n")?;
        let name = std::str::from_utf8(&value[..end]).ok()?.trim();
        (!name.is_empty() && name.len() <= 256).then(|| Self {
            name: if name.rsplit('/').next().unwrap_or(name).contains(':') {
                name.to_owned()
            } else {
                format!("{name}:latest")
            },
            context: None,
            options_unverified: true,
            digest: None,
            forecast_bytes: None,
            not_installed: false,
        })
    }

    /// Whether Ollama runs this model at a compatible context: an explicit `options.num_ctx`
    /// other than the resident `context_length` makes Ollama reload the runner. A bare name
    /// (`gemma3`) means `gemma3:latest` to Ollama, so residency, busy marks and the audit all key
    /// on that tag.
    async fn observe(&mut self, state: &ProxyState) -> bool {
        if self.options_unverified {
            return false;
        }
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
        } else if !self
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&self.name)
            .contains(':')
        {
            self.name.push_str(":latest");
        }
        self.transition_compatible(&ps)
    }

    async fn hydrate(&mut self, state: &ProxyState) {
        let forecast = if let Some(hooks) = &state.hooks {
            hooks.forecast(&self.name, self.context).await
        } else {
            None
        };
        let forecast = match forecast {
            Some(forecast) => Some(forecast),
            None => installed_forecast(state, &self.name).await,
        };
        if let Some(forecast) = forecast {
            self.context = self.context.or(forecast.context_tokens);
            self.digest = forecast.digest;
            self.forecast_bytes = Some(forecast.required_host_bytes);
        } else {
            self.not_installed = not_listed(state, &self.name).await;
        }
    }

    /// Both initial sharing and an early transition release require known identity and exact
    /// effective context. Unreported runtime controls and unknown defaults remain conservative.
    fn transition_compatible(&self, ps: &serde_json::Value) -> bool {
        !self.options_unverified
            && crate::ollama_http::ps_entries(ps, &self.name).any(|entry| {
                self.context
                    .filter(|context| *context > 0)
                    .is_some_and(|context| entry["context_length"].as_u64() == Some(context))
                    && self.digest.as_deref().is_some_and(|digest| {
                        !digest.is_empty() && entry["digest"].as_str() == Some(digest)
                    })
                    // Raw callers state no placement evidence requirement, so the shared
                    // rule imposes none here; it is the managed rule's `Any`.
                    && PlacementRule::for_backend(Backend::Gpu, false, false).satisfied_by(entry)
            })
    }
}

/// A typed refusal: this generation's allocation could not be forecast, so it is not admitted.
fn model_forecast_unavailable(reason: &str) -> Response<Body> {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({"error": reason, "code":"model_forecast_unavailable", "retry_after_seconds":1})
            .to_string(),
    );
    response
        .headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    response
}

fn json_response(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("static response is valid")
}

/// Whether reading a body failed on the length limit (`to_bytes` wraps the limit's
/// [`http_body_util::LengthLimitError`]) rather than on a broken or aborted upload.
fn exceeds_length_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|error| {
        error
            .downcast_ref::<http_body_util::LengthLimitError>()
            .is_some()
    })
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

/// Too many request bodies are buffered at once; this one was never read.
fn body_budget_exhausted(error: body_budget::BodyBudgetExhausted) -> Response<Body> {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({"error": error.to_string(), "code": "request_body_budget_exhausted", "retry_after_seconds": 1})
            .to_string(),
    );
    response
        .headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    response
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
    // Reserve the body's size before reading it, so concurrent uploads cannot each hold the full
    // request limit in memory. A body without a declared length reserves the limit. The
    // reservation lives as long as the buffered bytes: until this handler returns.
    let expected =
        declared.unwrap_or_else(|| expected_body_bytes(&body.size_hint(), state.max_request_bytes));
    let budget_wait = state
        .admission
        .as_ref()
        .map_or(state.raw_queue_wait, RawAdmission::wait);
    let _body_reservation = match state.body_budget.reserve(expected, budget_wait).await {
        Ok(reservation) => reservation,
        Err(exhausted) => return body_budget_exhausted(exhausted),
    };
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
            return if exceeds_length_limit(&error) {
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
    // Register synchronously before the bounded raw queue, protecting queued demand. Wait
    // for an existing unload only after a slot is held, so there is no extra unbounded queue.
    let mut in_use = state
        .hooks
        .as_ref()
        .zip(model.as_ref().filter(|_| generation))
        .map(|(hooks, model)| hooks.begin(&model.name));
    // Slot first, then the execution lock, so a queued raw request never holds the lock while
    // waiting for a slot.
    let admission = if kind == RawRequestKind::Unload {
        Some(&state.unload_admission)
    } else {
        state.admission.as_ref().filter(|_| generation)
    };
    let permit = match admission {
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
    if let Some((hooks, model)) = state.hooks.as_ref().zip(model.as_ref()) {
        let ready = if generation {
            tokio::time::timeout_at(deadline, hooks.ready(&model.name))
                .await
                .is_ok()
        } else if let Ok(lease) =
            tokio::time::timeout_at(deadline, hooks.acquire_unload(&model.name)).await
        {
            in_use = Some(lease);
            true
        } else {
            false
        };
        if !ready {
            count("rejected_busy");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "model lifecycle is busy; retry after its current work finishes",
                1,
            );
        }
    }
    if kind == RawRequestKind::Unload && model.is_some() && in_use.is_some() {
        // Once dispatched, an explicit unload keeps its lifecycle lease even if the caller
        // disconnects. Its small response is buffered unchanged; cleanup has a fixed bound.
        let cleanup = tokio::spawn(forward_owned_unload(
            state.clone(),
            parts,
            body_bytes,
            model,
            permit,
            in_use,
        ));
        return cleanup.await.unwrap_or_else(|error| json_response(StatusCode::BAD_GATEWAY,
            serde_json::json!({"error":format!("unload supervisor failed: {error}"),"code":"unload_outcome_unknown"}).to_string()));
    }
    if generation
        && let Some(model) = model.as_mut()
        && tokio::time::timeout(RAW_FORECAST_WAIT, model.hydrate(&state))
            .await
            .is_err()
    {
        return model_forecast_unavailable("model metadata forecast timed out");
    }
    // A generation whose model cannot be resolved may still load one (a multipart body Ollama
    // reads but this proxy does not), and nothing could size or serialize that load. Refuse it
    // rather than admit it with a zero-byte forecast. A well-formed JSON body that names no
    // usable model is different: Ollama rejects it before scheduling, so it can load nothing.
    if generation
        && model.is_none()
        && serde_json::from_slice::<serde_json::Value>(&body_bytes).is_err()
    {
        count("rejected_pressure");
        return model_forecast_unavailable(
            "request model could not be resolved, so its allocation cannot be forecast",
        );
    }
    // A raw generation on a runner Ollama already holds at a compatible identity takes no
    // backend lock: its model use lease keeps the runner from being evicted while it decodes,
    // and a cold load of another model does not wait for it. Only a load owns the load lock,
    // until its runner is observed ready. Residency is observed again after the lock is won, so
    // a request that queued behind its own model's load shares the loaded runner.
    let mut observed = None;
    let mut load = if generation
        && model.is_some()
        && let Some(lock) = state.load_lock.as_ref()
    {
        let resident_now = match (&state.hooks, &mut model) {
            (Some(_), Some(model)) => Some(model.observe(&state).await),
            _ => None,
        };
        if resident_now == Some(true) {
            observed = Some(true);
            None
        } else if let Ok(guard) =
            tokio::time::timeout_at(deadline, Arc::clone(lock).lock_owned()).await
        {
            let resident_now = match (&state.hooks, &mut model) {
                (Some(_), Some(model)) => Some(model.observe(&state).await),
                _ => None,
            };
            observed = resident_now;
            (resident_now != Some(true)).then_some(guard)
        } else {
            count("rejected_busy");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy busy: another model load is active; use managed /_freellama/v1/tasks to queue",
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
    let mut resource = if generation {
        match tokio::time::timeout(
            RAW_FORECAST_WAIT,
            raw_resource_permit(&state, model.as_mut(), resident),
        )
        .await
        {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(error)) => {
                count("rejected_pressure");
                let message = if error.receipt.status == "model_forecast_unavailable" {
                    "local model allocation could not be forecast; retry when Ollama model metadata is available".to_owned()
                } else {
                    error.to_string()
                };
                let mut refusal = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({"error": message, "code": error.receipt.status, "resource_admission": error.receipt})
                        .to_string(),
                );
                refusal
                    .headers_mut()
                    .insert("retry-after", axum::http::HeaderValue::from_static("2"));
                return refusal;
            }
            Err(_) => {
                count("rejected_pressure");
                return refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "raw resource forecast or admission exceeded its deadline",
                    1,
                );
            }
        }
    } else {
        None
    };
    let mut transition_watch = if load.is_some() {
        model
            .as_ref()
            .filter(|model| model.digest.is_some())
            .map(|model| raw_transition_watch(&state, model.clone()))
    } else {
        None
    };
    // Nonstream Ollama responses can withhold headers until decoding ends. Observe readiness
    // while forwarding too, so those requests release the load lock as soon as their runner is
    // ready; an unfinished watch follows a streaming response into its body below.
    let forwarded = forward_inner(&state, &parts, &body_bytes);
    tokio::pin!(forwarded);
    let response = if let Some(watch) = transition_watch.as_mut() {
        tokio::select! {
            response = &mut forwarded => response,
            () = watch => {
                transition_watch = None;
                load = None;
                if crate::platform::host_has_unified_memory()
                    && let Some(resource) = resource.as_mut()
                {
                    resource.release();
                }
                forwarded.await
            }
        }
    } else {
        forwarded.await
    };
    match response {
        Ok(response) => {
            count("forwarded");
            let mut usage = None;
            if response.status().is_success()
                && let (Some(hooks), Some(model)) = (&state.hooks, &model)
            {
                if generation {
                    let reuse_unverified = resident == Some(false);
                    usage = Some(usage_tap(
                        hooks,
                        "gpu",
                        &model.name,
                        &parts,
                        reuse_unverified,
                        started,
                        expects_terminal(parts.uri.path(), &body_bytes),
                    ));
                } else if resident == Some(true) {
                    // Only a runner that was loaded is unloaded; anything else changed nothing.
                    hooks.unload_accepted(&model.name);
                }
            }
            let (response_parts, body) = response.into_parts();
            Response::from_parts(
                response_parts,
                Body::new(AdmittedBody {
                    inner: body,
                    permit,
                    load,
                    resource,
                    in_use,
                    usage,
                    transition_watch,
                }),
            )
        }
        Err(error) => {
            count("upstream_error");
            drop((permit, load, resource));
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
    reuse_unverified: bool,
    started: Instant,
    requires_terminal: bool,
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
        requires_terminal,
        finished: Some(Box::new(move |usage, outcome| {
            if outcome == RawOutcome::Complete && backend == "gpu" {
                hooks.served(&model, reuse_unverified);
            }
            hooks.finished(RawFinished {
                backend: &backend,
                model: &model,
                path: &path,
                caller: caller.as_deref(),
                reuse_unverified,
                usage,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                outcome,
            });
        })),
    }
}

fn expects_terminal(path: &str, body: &[u8]) -> bool {
    let body: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    body["stream"]
        .as_bool()
        .unwrap_or(matches!(path, "/api/generate" | "/api/chat"))
}

async fn forward_owned_unload(
    state: ProxyState,
    parts: axum::http::request::Parts,
    body: Bytes,
    model: Option<RawModel>,
    permit: Option<admission::RawPermit>,
    in_use: Option<Box<dyn Send>>,
) -> Response<Body> {
    let _leases = (permit, in_use);
    let forwarded = tokio::time::timeout(Duration::from_secs(5), async {
        let response = forward_inner(&state, &parts, &body).await?;
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, state.max_request_bytes).await?;
        Ok::<_, anyhow::Error>(Response::from_parts(parts, Body::from(bytes)))
    })
    .await;
    let response = match forwarded {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => json_response(StatusCode::BAD_GATEWAY, serde_json::json!({"error":error.to_string(),"code":"unload_outcome_unknown"}).to_string()),
        Err(_) => json_response(StatusCode::GATEWAY_TIMEOUT, serde_json::json!({"error":"unload request timed out; upstream outcome is unknown","code":"unload_outcome_unknown"}).to_string()),
    };
    if let Some((hooks, model)) = state.hooks.as_ref().zip(model.as_ref()) {
        if response.status().is_success() {
            hooks.unload_accepted(&model.name);
        }
        // Even a failed exchange may have delivered the unload. Keep ownership until absence
        // is confirmed or this bounded unknown-outcome window expires.
        let confirmation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(response) = state
                    .client
                    .get(format!("{}/api/ps", state.upstream.trim_end_matches('/')))
                    .send()
                    .await
                    && response.status().is_success()
                    && let Ok(ps) = response.json::<serde_json::Value>().await
                    && ps["models"].as_array().is_some_and(|entries| {
                        entries
                            .iter()
                            .all(|entry| crate::ollama_http::ps_entry_name(entry).is_some())
                    })
                    && crate::ollama_http::find_ps_entry(&ps, &model.name).is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        if confirmation.is_ok() {
            hooks.unload_confirmed(&model.name);
        }
    }
    state.resource_governor.invalidate().await;
    response
}

fn raw_transition_watch(state: &ProxyState, model: RawModel) -> admission::TransitionWatch {
    let client = state.client.clone();
    let upstream = state.upstream.clone();
    let hooks = state.hooks.clone();
    Box::pin(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(response) = client
                .get(format!("{}/api/ps", upstream.trim_end_matches('/')))
                .send()
                .await
                && let Ok(ps) = response.json::<serde_json::Value>().await
                && model.transition_compatible(&ps)
            {
                if let Some(hooks) = &hooks {
                    hooks.runner_ready();
                }
                return;
            }
        }
    })
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
    let requires_terminal = expects_terminal(parts.uri.path(), &body);
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
        .map(|hooks| {
            usage_tap(
                hooks,
                &upstream.name,
                model,
                parts,
                false,
                started,
                requires_terminal,
            )
        });
    builder
        .header("x-freellama-proxy", "1")
        .header("x-freellama-upstream", upstream.name.as_str())
        .body(Body::new(AdmittedBody {
            inner: Body::from_stream(response.bytes_stream()),
            permit: None,
            load: None,
            resource,
            in_use: None,
            usage,
            transition_watch: None,
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
    use super::{RawModel, RawRequestKind, classify};
    use reqwest::Method;

    #[test]
    fn raw_initial_reuse_and_transition_release_require_exact_known_identity_and_context() {
        let mut model = RawModel::parse(br#"{"model":"m","options":{"num_ctx":4096}}"#).unwrap();
        let exact =
            serde_json::json!({"models":[{"name":"m:latest","digest":"a","context_length":4096}]});
        assert!(!model.transition_compatible(&exact));
        model.digest = Some("a".into());
        assert!(model.transition_compatible(&exact));
        model.digest = Some("replacement".into());
        assert!(
            !model.transition_compatible(&exact),
            "a replaced installed tag cannot reuse its old resident runner"
        );
        model.digest = Some("a".into());
        for (digest, context) in [("b", 4096), ("a", 2048), ("a", 8192), ("a", 0)] {
            assert!(!model.transition_compatible(&serde_json::json!({"models":[{"name":"m:latest","digest":digest,"context_length":context}]})));
        }
        model.context = None;
        assert!(!model.transition_compatible(&exact));
        model.context = Some(4096);
        model.options_unverified = true;
        assert!(!model.transition_compatible(&exact));
    }

    #[test]
    fn a_multipart_audio_request_names_its_model() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\nRIFF\r\n--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nlfm3-audio:2b\r\n--b--\r\n";
        let model = RawModel::parse(body).expect("model field");
        assert_eq!(model.name, "lfm3-audio:2b");
        assert!(RawModel::parse(b"--b\r\nname=\"file\"\r\n\r\nRIFF\r\n--b--").is_none());
        assert_eq!(
            RawModel::parse(br#"{"model":"phonon:en","input":"hi"}"#).map(|model| model.name),
            Some("phonon:en".to_owned())
        );
    }

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

    /// A full buffered-body budget refuses a body it would have to read, before reading it, and
    /// still forwards requests with no body.
    #[tokio::test]
    async fn a_full_body_budget_refuses_uploads_with_a_code_but_not_empty_requests() {
        use axum::{
            body::{Body, to_bytes},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;

        let upstream = axum::Router::new().fallback(|| async { "ok" });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let budget = super::BodyBudget::new(4096);
        let app = super::app(
            super::ProxyConfig::new("127.0.0.1:0", format!("http://{address}"), false)
                .with_body_budget(budget.clone()),
        )
        .unwrap();
        let held = budget
            .reserve(4096, std::time::Duration::ZERO)
            .await
            .unwrap();
        let upload = || {
            Request::post("/api/show")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"m"}"#))
                .unwrap()
        };
        let refused = app.clone().oneshot(upload()).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.headers()["retry-after"], "1");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(refused.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["code"], "request_body_budget_exhausted");
        let empty = app
            .clone()
            .oneshot(Request::get("/api/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            empty.status(),
            StatusCode::OK,
            "an empty body reserves nothing"
        );
        drop(held);
        let forwarded = app.oneshot(upload()).await.unwrap();
        assert_eq!(forwarded.status(), StatusCode::OK);
        assert_eq!(
            budget.receipt()["available_bytes"],
            4096,
            "returned with the body"
        );
        server.abort();
    }

    /// A body without a declared length (chunked) that passes the limit is refused with the same
    /// 413 as one whose `content-length` declares it; a body that fails for another reason is a
    /// 400. Detection follows the error's source chain to `LengthLimitError`, not its text.
    #[tokio::test]
    async fn an_oversized_chunked_body_is_refused_with_413_and_a_broken_one_with_400() {
        use axum::{
            body::{Body, Bytes, to_bytes},
            http::{Request, StatusCode},
        };
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        use tower::ServiceExt as _;

        /// Frames without a size hint, ending in `error` when one is given.
        struct Chunked {
            frames: Vec<Bytes>,
            error: Option<std::io::Error>,
        }
        impl http_body::Body for Chunked {
            type Data = Bytes;
            type Error = std::io::Error;
            fn poll_frame(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
                let this = self.get_mut();
                Poll::Ready(if this.frames.is_empty() {
                    this.error.take().map(Err)
                } else {
                    Some(Ok(http_body::Frame::data(this.frames.remove(0))))
                })
            }
        }
        let app = super::app(
            super::ProxyConfig::new("127.0.0.1:0", "http://127.0.0.1:9", false)
                .with_body_budget(super::BodyBudget::new(4096))
                .with_max_request_bytes(16),
        )
        .unwrap();
        let send = |body: Body, declared: Option<usize>| {
            let mut request =
                Request::post("/api/generate").header("content-type", "application/json");
            if let Some(bytes) = declared {
                request = request.header("content-length", bytes);
            }
            app.clone().oneshot(request.body(body).unwrap())
        };
        let read = |response: axum::response::Response| async move {
            let status = response.status();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            (status, body)
        };
        let expected = serde_json::json!({
            "error": "request body is over 16 bytes; raise FREELLAMA_MAX_REQUEST_BYTES to accept it"
        });
        let chunked = Body::new(Chunked {
            frames: vec![
                Bytes::from_static(b"0123456789"),
                Bytes::from_static(b"0123456789"),
            ],
            error: None,
        });
        assert_eq!(
            read(send(chunked, None).await.unwrap()).await,
            (StatusCode::PAYLOAD_TOO_LARGE, expected.clone())
        );
        let declared = Body::from(vec![b'x'; 20]);
        assert_eq!(
            read(send(declared, Some(20)).await.unwrap()).await,
            (StatusCode::PAYLOAD_TOO_LARGE, expected)
        );
        let broken = Body::new(Chunked {
            frames: vec![Bytes::from_static(b"{")],
            error: Some(std::io::Error::other("length limit exceeded")),
        });
        assert_eq!(
            read(send(broken, None).await.unwrap()).await,
            (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "could not read request body"})
            ),
            "an upload error that merely mentions the limit is not an oversized body"
        );
    }
}
