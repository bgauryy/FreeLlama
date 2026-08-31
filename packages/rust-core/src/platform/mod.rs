//! Machine-aware local-model discovery, routing, sessions, and task execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use crate::{
    model_bench::Capability,
    proxy::{self, ProxyConfig},
    recommend::{
        InstallPlan, InstallationPlanRequest, RecommendationCatalog, installation_plans,
        load_catalog,
    },
};

mod discovery;
mod intent;
mod routing;

pub use discovery::MachineProfile;
pub use intent::{RouteIntent, intent_schema, normalize_route_intent, parse_route_intent};
pub use routing::{
    CatalogModel, Objective, RouteDecision, RouteEvidence, RouteInput, SessionAffinity, TaskKind,
    select_route,
};

// Private helpers the server plane reuses from the pure routing/intent/discovery modules.
use discovery::{load_benchmark, load_policies, machine_profile, parse_capability};
use intent::intent_system_prompt;
use routing::{requested_context, requirements};

const API_ROOT: &str = "/_freellama/v1";
type CatalogCache = Arc<RwLock<Option<(Instant, Vec<CatalogModel>)>>>;

#[derive(Debug, Clone)]
pub struct PlatformConfig {
    pub listen: String,
    pub upstream: String,
    pub benchmark_report: Option<PathBuf>,
    pub policy_file: Option<PathBuf>,
    pub recommendation_catalog: Option<PathBuf>,
    pub intent_model: String,
    /// Concurrent managed tasks allowed against Ollama, in cost units. `None` falls back to
    /// `FREELLAMA_MAX_CONCURRENT_TASKS`, then to 8.
    pub max_concurrent_tasks: Option<usize>,
    /// Longest a task may queue for an admission slot before being refused with 503. `None` falls
    /// back to `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, then to 120s.
    pub max_queue_wait: Option<Duration>,
}

impl PlatformConfig {
    #[must_use]
    pub fn new(
        listen: impl Into<String>,
        upstream: impl Into<String>,
        benchmark_report: Option<PathBuf>,
        policy_file: Option<PathBuf>,
        intent_model: impl Into<String>,
    ) -> Self {
        Self {
            listen: listen.into(),
            upstream: upstream.into(),
            benchmark_report,
            policy_file,
            recommendation_catalog: None,
            intent_model: intent_model.into(),
            max_concurrent_tasks: None,
            max_queue_wait: None,
        }
    }

    /// Cap how long a task may queue for admission before being refused.
    ///
    /// Exposed on the config, not env-only, so the refusal path can be tested without mutating
    /// process environment — which Rust 2024 makes `unsafe`, and this crate denies `unsafe`.
    #[must_use]
    pub fn with_max_queue_wait(mut self, wait: Duration) -> Self {
        self.max_queue_wait = Some(wait);
        self
    }

    /// Bound how many managed tasks may be in flight against Ollama at once.
    ///
    /// Set it to match `OLLAMA_NUM_PARALLEL`. Ollama's own default is 1, so a higher number here
    /// does not buy parallel decoding — it keeps the pipe full and bounds the burst. Exposed on the
    /// config (not env-only) so a test or an embedding application can set it without mutating
    /// process environment.
    #[must_use]
    pub fn with_max_concurrent_tasks(mut self, slots: usize) -> Self {
        self.max_concurrent_tasks = Some(slots.max(1));
        self
    }

    /// The admission budget this config will actually run with, after the
    /// `FREELLAMA_MAX_CONCURRENT_TASKS` fallback and the semaphore's own ceiling.
    ///
    /// Public because the CLI prints the budget at startup: reading `max_concurrent_tasks`
    /// directly there reported the hardcoded default whenever the env var was the thing setting
    /// it, so the operator was told 8 while the server ran with something else.
    #[must_use]
    pub fn resolved_max_concurrent_tasks(&self) -> usize {
        self.max_concurrent_tasks
            .unwrap_or_else(max_concurrent_tasks)
            // `Semaphore::new` panics above `MAX_PERMITS`, and this number can come from an env
            // var — a startup panic is the wrong answer to a typo'd `FREELLAMA_MAX_CONCURRENT_TASKS`.
            .clamp(1, Semaphore::MAX_PERMITS)
    }

    #[must_use]
    pub fn with_recommendation_catalog(mut self, path: impl Into<PathBuf>) -> Self {
        self.recommendation_catalog = Some(path.into());
        self
    }

    /// Validate the loopback-only platform boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or non-loopback listener or recursive upstream.
    pub fn validate(&self) -> Result<()> {
        let listen: SocketAddr = self.listen.parse().context("invalid --listen address")?;
        ensure!(
            listen.ip().is_loopback(),
            "platform listener must be loopback"
        );
        ProxyConfig::new(&self.listen, &self.upstream, false)
            .validate()
            .and_then(|()| {
                ensure!(
                    !self.intent_model.trim().is_empty(),
                    "intent model must not be empty"
                );
                Ok(())
            })
    }
}

#[derive(Clone)]
struct PlatformState {
    client: Client,
    upstream: String,
    benchmark: Arc<BTreeMap<String, BTreeMap<Capability, f64>>>,
    policies: Arc<BTreeMap<TaskKind, Vec<String>>>,
    recommendations: Arc<RecommendationCatalog>,
    sessions: Arc<RwLock<SessionAffinity>>,
    catalog_cache: CatalogCache,
    intent_model: String,
    managed_execution: Arc<RwLock<()>>,
    /// Bounds how many managed tasks may be in flight against Ollama at once.
    ///
    /// `managed_execution` alone does not do this: resident tasks take a *shared* permit, so any
    /// number of them could be admitted together and pile into Ollama. Ollama then queues them
    /// (`OLLAMA_MAX_QUEUE`, default 512) and rejects the overflow with HTTP 503 — and every queued
    /// request spends its own 900s client budget waiting its turn, so a burst converts into
    /// timeouts rather than backpressure. A semaphore turns that into a bounded, FIFO wait that the
    /// caller can actually see.
    task_slots: Arc<Semaphore>,
    slots_total: usize,
    queue_wait: Duration,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: error.to_string(),
        }
    }

    fn upstream(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

/// Build the localhost platform and its Ollama-compatible fallback.
///
/// # Errors
///
/// Returns an error for unsafe configuration, unreadable benchmark evidence, or HTTP setup.
pub fn app(config: &PlatformConfig) -> Result<Router> {
    config.validate()?;
    let benchmark = load_benchmark(config.benchmark_report.as_ref())?;
    let policies = load_policies(config.policy_file.as_ref())?;
    let recommendation_catalog = load_catalog(config.recommendation_catalog.as_ref())?;
    let slots_total = config.resolved_max_concurrent_tasks();
    let state = PlatformState {
        // A client-level backstop, not a nicety. `forward_managed_task` holds the
        // `managed_execution` write lock across its upstream call, so an untimed request against
        // a wedged Ollama would hold that exclusive lock forever and block every subsequent
        // managed task — one hung request deadlocking the whole managed plane. Generous enough
        // for a real generation (Ollama's own OLLAMA_LOAD_TIMEOUT is 5m for the load alone);
        // cheap discovery calls take a much shorter per-request timeout below.
        client: Client::builder()
            .timeout(platform_task_timeout())
            .build()
            .context("build platform HTTP client")?,
        upstream: config.upstream.clone(),
        benchmark: Arc::new(benchmark),
        policies: Arc::new(policies),
        recommendations: Arc::new(recommendation_catalog),
        sessions: Arc::new(RwLock::new(SessionAffinity::default())),
        catalog_cache: Arc::new(RwLock::new(None)),
        intent_model: config.intent_model.clone(),
        managed_execution: Arc::new(RwLock::new(())),
        task_slots: Arc::new(Semaphore::new(slots_total)),
        slots_total,
        queue_wait: config.max_queue_wait.unwrap_or_else(max_queue_wait),
    };
    let platform = Router::new()
        .route(&format!("{API_ROOT}/health"), get(health))
        .route(&format!("{API_ROOT}/machine"), get(machine))
        .route(&format!("{API_ROOT}/models"), get(models))
        .route(
            &format!("{API_ROOT}/recommendations"),
            post(recommendations),
        )
        .route(&format!("{API_ROOT}/routes"), post(route))
        .route(&format!("{API_ROOT}/natural-routes"), post(natural_route))
        .route(&format!("{API_ROOT}/sessions"), post(create_session))
        .route(&format!("{API_ROOT}/tasks"), post(run_task))
        .with_state(state);
    let fallback = proxy::app(ProxyConfig::new(&config.listen, &config.upstream, false))?;
    Ok(platform.merge(fallback))
}

/// Serve the localhost model platform until Ctrl-C.
///
/// # Errors
///
/// Returns an error when binding, configuration, or serving fails.
pub async fn serve(config: PlatformConfig) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind platform at {}", config.listen))?;
    let app = app(&config)?;
    println!("FreeLlama platform listening on http://{}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serve platform")
}

/// Liveness plus a load-shedding signal.
///
/// An orchestrating agent deciding "delegate, queue, or do it myself" needs a cheap read-only
/// answer to "will a task be admitted right now?". Without it the only way to find out is to
/// submit and possibly eat a 120s queue wait or a 503. `slots_available` is a snapshot — racy by
/// nature, advisory by design — but 0 here means "expect to queue", which is exactly the decision
/// input a caller needs. Standard readiness-endpoint practice, kept on `/health` rather than a new
/// tool so the surface does not grow.
async fn health(State(state): State<PlatformState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "admission": {
            "slots_total": state.slots_total,
            "slots_available": state.task_slots.available_permits(),
            "max_queue_wait_seconds": state.queue_wait.as_secs(),
            "costs": {"embedding": 1, "chat": 2, "vision": 4},
        },
    }))
}

async fn machine(State(state): State<PlatformState>) -> Json<MachineProfile> {
    Json(machine_profile(&state.upstream))
}

async fn models(State(state): State<PlatformState>) -> Result<Json<Value>, ApiError> {
    let models = discover_models(&state).await?;
    Ok(Json(json!({"models": models})))
}

#[derive(Debug, Serialize)]
pub struct RecommendationResponse {
    pub request: RouteInput,
    pub required_capabilities: BTreeSet<Capability>,
    pub requested_context_tokens: u64,
    pub machine: MachineProfile,
    pub installed_route: Option<RouteDecision>,
    pub installed_route_error: Option<String>,
    pub install_plans: Vec<InstallPlan>,
    pub catalog_reviewed_at: Option<String>,
    pub catalog_review_due_at: Option<String>,
    pub side_effects_performed: bool,
}

async fn recommendations(
    State(state): State<PlatformState>,
    Json(input): Json<RouteInput>,
) -> Result<Json<RecommendationResponse>, ApiError> {
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    if let Some(id) = input.session_id.as_deref() {
        if !sessions.contains(id) {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: "session does not exist".to_owned(),
            });
        }
    }
    let route_result = select_route(&input, &models, &sessions);
    let (installed_route, installed_route_error) = match route_result {
        Ok(route) => (Some(route), None),
        Err(error) => (None, Some(error.to_string())),
    };
    drop(sessions);
    let machine = machine_profile(&state.upstream);
    let required_capabilities = requirements(&input);
    let requested_context_tokens = requested_context(&input);
    let installed_models = models
        .iter()
        .map(|model| model.name.clone())
        .collect::<BTreeSet<_>>();
    let install_plans = installation_plans(
        &state.recommendations,
        &InstallationPlanRequest {
            task: input.task,
            explicit_model: input.model.as_deref(),
            required_capabilities: &required_capabilities,
            requested_context: requested_context_tokens,
            installed_models: &installed_models,
            unified_memory_bytes: machine.unified_memory_bytes,
            available_disk_bytes: machine.available_disk_bytes,
        },
    );
    Ok(Json(RecommendationResponse {
        request: input,
        required_capabilities,
        requested_context_tokens,
        machine,
        installed_route,
        installed_route_error,
        install_plans,
        catalog_reviewed_at: state.recommendations.reviewed_at.clone(),
        catalog_review_due_at: state.recommendations.review_due_at.clone(),
        side_effects_performed: false,
    }))
}

async fn route(
    State(state): State<PlatformState>,
    Json(input): Json<RouteInput>,
) -> Result<Json<RouteDecision>, ApiError> {
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    if let Some(id) = input.session_id.as_deref() {
        if !sessions.contains(id) {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: "session does not exist".to_owned(),
            });
        }
    }
    let decision = select_route(&input, &models, &sessions).map_err(ApiError::bad_request)?;
    drop(sessions);
    if let Some(id) = input.session_id.as_deref() {
        state
            .sessions
            .write()
            .await
            .bind(id, &decision.selected_model);
    }
    Ok(Json(decision))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NaturalRouteInput {
    text: String,
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct NaturalRouteResponse {
    interpreter_model: String,
    interpreter_ms: u64,
    intent: RouteIntent,
    guard_adjustments: Vec<String>,
    route: RouteDecision,
}

async fn natural_route(
    State(state): State<PlatformState>,
    Json(input): Json<NaturalRouteInput>,
) -> Result<Json<NaturalRouteResponse>, ApiError> {
    let text = input.text.trim();
    if text.is_empty() || text.len() > 16_384 {
        return Err(ApiError::bad_request(
            "text must contain between 1 and 16384 bytes",
        ));
    }
    let started = Instant::now();
    let response = state
        .client
        .post(format!("{}/api/chat", state.upstream.trim_end_matches('/')))
        .json(&json!({
            "model": state.intent_model,
            "messages": [
                {"role": "system", "content": intent_system_prompt()},
                {"role": "user", "content": text}
            ],
            "stream": false,
            "think": false,
            "format": intent_schema(),
            "keep_alive": "2m",
            "options": {"temperature": 0, "seed": 42, "num_predict": 96, "num_ctx": 2048}
        }))
        .send()
        .await
        .map_err(ApiError::upstream)?
        .error_for_status()
        .map_err(ApiError::upstream)?
        .json::<Value>()
        .await
        .map_err(ApiError::upstream)?;
    let content = response
        .pointer("/message/content")
        .and_then(Value::as_str)
        .context("intent model response has no message content")
        .map_err(ApiError::upstream)?;
    let interpreted = parse_route_intent(content).map_err(ApiError::upstream)?;
    let (intent, guard_adjustments) = normalize_route_intent(interpreted, text);
    let route_input = intent.clone().into_route_input(input.session_id);
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    if let Some(id) = route_input.session_id.as_deref() {
        if !sessions.contains(id) {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: "session does not exist".to_owned(),
            });
        }
    }
    let route = select_route(&route_input, &models, &sessions).map_err(ApiError::bad_request)?;
    drop(sessions);
    if let Some(id) = route_input.session_id.as_deref() {
        state.sessions.write().await.bind(id, &route.selected_model);
    }
    Ok(Json(NaturalRouteResponse {
        interpreter_model: state.intent_model,
        interpreter_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        intent,
        guard_adjustments,
        route,
    }))
}

async fn create_session(State(state): State<PlatformState>) -> Json<Value> {
    let id = state.sessions.write().await.create();
    Json(json!({"session_id": id, "affinity": "model"}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    #[serde(flatten)]
    route: RouteInput,
    #[serde(default)]
    messages: Vec<Value>,
    prompt: Option<String>,
    /// Base64-encoded images (Ollama's own `images` format — no data URI prefix) attached to the
    /// single message built from `prompt`. For multi-turn `messages`, put `images` directly on
    /// the relevant message object instead; this field only applies to the `prompt` convenience
    /// path.
    images: Option<Vec<String>>,
    input: Option<Value>,
    tools: Option<Value>,
    /// Overrides the default `keep_alive` sent to Ollama (any format Ollama itself accepts:
    /// `"5m"`, `"-1"` for infinite, `"0"` to unload immediately after this call). Defaults to
    /// `"5m"` when omitted, matching prior behavior exactly — callers that never set this see no
    /// change. A one-off embedding call is the clearest case for `"0"`: no reason to keep a model
    /// resident after a single vector is computed.
    keep_alive: Option<String>,
}

/// Wait for an admission slot sized to the task, or refuse.
///
/// Returns the held permit, the cost charged, and how long the caller queued.
async fn admit(
    state: &PlatformState,
    task: TaskKind,
) -> Result<(OwnedSemaphorePermit, u32, u128), ApiError> {
    let queued = Instant::now();
    let budget = u32::try_from(state.slots_total).unwrap_or(u32::MAX).max(1);
    let cost = task_cost(task).min(budget);
    let wait = state.queue_wait;
    let permit =
        match tokio::time::timeout(wait, Arc::clone(&state.task_slots).acquire_many_owned(cost))
            .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return Err(ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: "task admission is shutting down".to_owned(),
                });
            }
            Err(_) => {
                return Err(ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: format!(
                        "server busy: no admission slot within {}s (task cost {cost} of {budget} \
                         units). Retry, or raise --max-concurrent-tasks.",
                        wait.as_secs()
                    ),
                });
            }
        };
    Ok((permit, cost, queued.elapsed().as_millis()))
}

async fn run_task(
    State(state): State<PlatformState>,
    Json(input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    if let Some(id) = input.route.session_id.as_deref() {
        if !sessions.contains(id) {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: "session does not exist".to_owned(),
            });
        }
    }
    let decision = select_route(&input.route, &models, &sessions).map_err(ApiError::bad_request)?;
    drop(sessions);

    let keep_alive = input.keep_alive.unwrap_or_else(|| "5m".to_owned());
    let (path, body) = if matches!(input.route.task, TaskKind::Embedding) {
        let value = input
            .input
            .context("embedding task requires input")
            .map_err(ApiError::bad_request)?;
        (
            "/api/embed",
            json!({
                "model": decision.selected_model,
                "input": value,
                "keep_alive": keep_alive,
                "options": decision.options,
            }),
        )
    } else {
        let messages = if input.messages.is_empty() {
            let mut message = json!({
                "role": "user",
                "content": input.prompt.context("task requires prompt or messages").map_err(ApiError::bad_request)?
            });
            if let Some(images) = input.images {
                message["images"] = json!(images);
            }
            vec![message]
        } else {
            input.messages
        };
        let mut body = json!({
            "model": decision.selected_model,
            "messages": messages,
            "stream": false,
            "keep_alive": keep_alive,
            "options": decision.options,
        });
        if !decision.think.is_null() {
            body["think"] = decision.think.clone();
        }
        if let Some(tools) = input.tools {
            body["tools"] = tools;
        }
        ("/api/chat", body)
    };
    // Slot first, THEN the transition lock — in both branches. The order matters: if the
    // non-resident path took the write lock before its slot while resident tasks held slots and
    // waited on the read lock, the two would deadlock. One consistent order removes that entirely.
    let (slot, cost, queue_wait_ms) = admit(&state, decision.task).await?;

    // Bind session affinity only AFTER admission succeeds. Binding at routing time meant a task
    // refused for lack of an admission slot had already pinned the session to a model: the caller
    // saw a 503 and reasonably concluded nothing happened, while every later request in that
    // session had silently been redirected. State changes belong after the last thing that can
    // refuse, not before it.
    if let Some(id) = input.route.session_id.as_deref() {
        state
            .sessions
            .write()
            .await
            .bind(id, &decision.selected_model);
    }

    if decision.resident {
        let _permit = state.managed_execution.read().await;
        forward_managed_task(
            &state,
            decision,
            path,
            body,
            "resident_shared",
            slot,
            queue_wait_ms,
            cost,
        )
        .await
    } else {
        let _permit = state.managed_execution.write().await;
        forward_managed_task(
            &state,
            decision,
            path,
            body,
            "nonresident_transition_exclusive",
            slot,
            queue_wait_ms,
            cost,
        )
        .await
    }
}

/// POST JSON upstream, retrying transient failures on the same backoff schedule the passthrough
/// proxy uses (`proxy::retry_delay`).
///
/// The managed-task path was the one retry-capable caller that had no retries: an Ollama 500 —
/// which it returns under load-model contention, the exact condition managed routing creates —
/// failed the whole task, while the byte-identical request through the passthrough proxy would
/// have survived it. The asymmetry was worse than it looks, because the caller holds the
/// `managed_execution` admission permit across this call: failing bare also threw away an
/// exclusive slot it had already queued for, so the retry it needed was the expensive one to skip.
async fn post_json_with_retries(
    state: &PlatformState,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let url = format!("{}{path}", state.upstream.trim_end_matches('/'));
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let more_attempts = attempt < proxy::MAX_ATTEMPTS;
        match state.client.post(&url).json(body).send().await {
            Ok(response) if response.status().is_server_error() && more_attempts => {
                eprintln!(
                    "managed task retry attempt={attempt} status={} path={path}",
                    response.status()
                );
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Ok(response) => {
                let status = response.status();
                let bytes = response.bytes().await.map_err(ApiError::upstream)?;
                // A failing Ollama does not always answer in JSON (a wedged runner can return a
                // plain-text or HTML body). Parsing strictly here used to convert a truthful 500
                // into a misleading "decode error", hiding the real upstream status from the
                // caller — so fall back to carrying the body through as text.
                let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(
                    |_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_owned() }),
                );
                return Ok((status, value));
            }
            // A timeout is NOT a transient hiccup here. This client's per-attempt budget is
            // `platform_task_timeout()` (900s by default), and the caller holds both an admission
            // slot and — on the non-resident path — the exclusive `managed_execution` write lock
            // across every attempt. Retrying a timeout would therefore hold the whole managed
            // plane for up to 3 x 900s, which is exactly the "one hung request deadlocks every
            // subsequent managed task" failure the client timeout was added to prevent. Connection
            // errors are still retried: those fail fast and cost nothing to re-pay.
            Err(error) if more_attempts && !error.is_timeout() => {
                eprintln!("managed task retry attempt={attempt} error={error:#} path={path}");
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Err(error) => return Err(ApiError::upstream(error)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_managed_task(
    state: &PlatformState,
    decision: RouteDecision,
    path: &str,
    body: Value,
    admission_mode: &str,
    // Held for the duration of the upstream call, then dropped. Taking it by value rather than by
    // reference makes the lifetime the compiler's problem instead of a comment's.
    slot: OwnedSemaphorePermit,
    queue_wait_ms: u128,
    cost: u32,
) -> Result<Json<Value>, ApiError> {
    let (status, value) = post_json_with_retries(state, path, &body).await?;
    if !status.is_success() {
        return Err(ApiError {
            status,
            message: value.to_string(),
        });
    }
    let metrics = runtime_metrics(&value);
    let slots_total = state.slots_total;
    // Report throttling rather than hiding it. A caller that fans out embeddings needs to know it
    // is queueing here — otherwise the only symptom is latency it cannot attribute.
    let slots_available = state.task_slots.available_permits();
    drop(slot);
    Ok(Json(json!({
        "route": decision,
        "admission": {
            "mode": admission_mode,
            "queue_wait_ms": queue_wait_ms,
            "slots_total": slots_total,
            "slots_available_during_call": slots_available,
            "cost": cost,
        },
        "metrics": metrics,
        "response": value
    })))
}

/// Extract prompt-free performance fields from an Ollama response.
#[must_use]
pub fn runtime_metrics(response: &Value) -> Value {
    let prompt_count = response.get("prompt_eval_count").and_then(Value::as_u64);
    let prompt_duration = response.get("prompt_eval_duration").and_then(Value::as_u64);
    let output_count = response.get("eval_count").and_then(Value::as_u64);
    let output_duration = response.get("eval_duration").and_then(Value::as_u64);
    json!({
        "total_duration_ns": response.get("total_duration").and_then(Value::as_u64),
        "load_duration_ns": response.get("load_duration").and_then(Value::as_u64),
        "prompt_tokens": prompt_count,
        "prompt_duration_ns": prompt_duration,
        "prompt_tokens_per_second": tokens_per_second(prompt_count, prompt_duration),
        "output_tokens": output_count,
        "output_duration_ns": output_duration,
        "output_tokens_per_second": tokens_per_second(output_count, output_duration),
    })
}

fn tokens_per_second(count: Option<u64>, duration_ns: Option<u64>) -> Option<f64> {
    let (Some(count), Some(duration_ns)) = (count, duration_ns) else {
        return None;
    };
    if duration_ns == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(count as f64 * 1_000_000_000.0 / duration_ns as f64)
}

async fn discover_models(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    let cached = state
        .catalog_cache
        .read()
        .await
        .as_ref()
        .filter(|(saved, _)| saved.elapsed() < Duration::from_secs(30))
        .map(|(_, models)| models.clone());
    if let Some(mut models) = cached {
        refresh_residency(state, &mut models).await?;
        return Ok(models);
    }
    let tags = get_json(&state.client, &state.upstream, "/api/tags").await?;
    let ps = get_json(&state.client, &state.upstream, "/api/ps").await?;
    let resident = ps
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries = tags
        .get("models")
        .and_then(Value::as_array)
        .context("Ollama tags response has no models")
        .map_err(ApiError::upstream)?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .get("name")
            .or_else(|| entry.get("model"))
            .and_then(Value::as_str)
            .context("Ollama model has no name")
            .map_err(ApiError::upstream)?;
        let show = state
            .client
            .post(format!("{}/api/show", state.upstream.trim_end_matches('/')))
            .timeout(platform_control_timeout())
            .json(&json!({"model": name}))
            .send()
            .await
            .map_err(ApiError::upstream)?
            .error_for_status()
            .map_err(ApiError::upstream)?
            .json::<Value>()
            .await
            .map_err(ApiError::upstream)?;
        let capabilities = show
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(parse_capability)
            .collect();
        let advertised_context =
            show.get("model_info")
                .and_then(Value::as_object)
                .and_then(|info| {
                    info.iter()
                        .find(|(key, _)| key.ends_with(".context_length"))
                        .and_then(|(_, value)| value.as_u64())
                });
        let running = resident.iter().find(|running| {
            running
                .get("name")
                .or_else(|| running.get("model"))
                .and_then(Value::as_str)
                == Some(name)
        });
        models.push(CatalogModel {
            name: name.to_owned(),
            size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
            capabilities,
            advertised_context,
            resident: running.is_some(),
            resident_vram: running
                .and_then(|value| value.get("size_vram"))
                .and_then(Value::as_u64),
            benchmark: state.benchmark.get(name).cloned().unwrap_or_default(),
            policy_rank: state
                .policies
                .iter()
                .filter_map(|(task, candidates)| {
                    candidates
                        .iter()
                        .position(|candidate| candidate == name)
                        .map(|rank| (*task, rank))
                })
                .collect(),
        });
    }
    *state.catalog_cache.write().await = Some((Instant::now(), models.clone()));
    Ok(models)
}

async fn refresh_residency(
    state: &PlatformState,
    models: &mut [CatalogModel],
) -> Result<(), ApiError> {
    let ps = get_json(&state.client, &state.upstream, "/api/ps").await?;
    let running = ps
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for model in models {
        let resident = running.iter().find(|entry| {
            entry
                .get("name")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                == Some(model.name.as_str())
        });
        model.resident = resident.is_some();
        model.resident_vram = resident
            .and_then(|value| value.get("size_vram"))
            .and_then(Value::as_u64);
    }
    Ok(())
}

/// Upper bound on a managed generation forwarded to Ollama. Overridable via
/// `FREELLAMA_TASK_TIMEOUT_SECONDS` — the same name the CLI and the NAPI layer read, so one
/// setting covers every path that can make a model generate.
/// Admission cost of a task, in slot units.
///
/// A flat per-request count is the wrong unit for local inference: a 274MB embedding and an 18GB
/// vision generation are not interchangeable, and counting them the same lets four vision calls in
/// where four embeddings belong. `FreeLlama` can weight them because it is the only layer that knows
/// what the task *is* — Ollama receives an opaque HTTP request and can only see memory after it has
/// already committed to loading something.
///
/// Deliberately coarse. These are relative costs, not a memory model; Ollama owns the real
/// memory-fit decision (`server/sched.go` evicts when a load is predicted to exceed 80% of free
/// memory) and duplicating that here would mean maintaining a worse copy of it.
fn task_cost(task: TaskKind) -> u32 {
    match task {
        // No sampling, tiny models, milliseconds. Batch freely.
        TaskKind::Embedding => 1,
        // Full generation, and on this machine the vision model is the 18GB one.
        TaskKind::Vision => 4,
        _ => 2,
    }
}

/// Longest a task may wait for an admission slot before being refused.
///
/// Ollama does not block when saturated: `getRunner` does a non-blocking send onto its pending
/// channel and returns `ErrMaxQueue` ("server busy, please try again") the instant it is full.
/// An unbounded wait here would convert that honest, actionable signal into an invisible pile-up,
/// where the only symptom is latency the caller cannot attribute. Match the upstream contract.
fn max_queue_wait() -> Duration {
    Duration::from_secs(
        std::env::var("FREELLAMA_MAX_QUEUE_WAIT_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(120),
    )
}

/// Total admission budget in slot units. Default 8 — enough for two concurrent generations or
/// eight embeddings.
///
/// Set this to match your `OLLAMA_NUM_PARALLEL`. Ollama's own default is **1**, so extra
/// concurrency here does not buy parallel decoding — it only keeps the pipe full and bounds the
/// burst. Raising `OLLAMA_NUM_PARALLEL` multiplies KV-cache memory by the context length, so raise
/// both together and check `models{view:"resident"}` after.
fn max_concurrent_tasks() -> usize {
    std::env::var("FREELLAMA_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8)
}

fn platform_task_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_TASK_TIMEOUT_SECONDS",
        crate::DEFAULT_TASK_TIMEOUT_SECS,
    )
}

/// Discovery calls (`/api/tags`, `/api/ps`, `/api/show`) read small in-memory state and must never
/// inherit the generation-sized budget above.
fn platform_control_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}

async fn get_json(client: &Client, upstream: &str, path: &str) -> Result<Value, ApiError> {
    client
        .get(format!("{}{path}", upstream.trim_end_matches('/')))
        .timeout(platform_control_timeout())
        .send()
        .await
        .map_err(ApiError::upstream)?
        .error_for_status()
        .map_err(ApiError::upstream)?
        .json()
        .await
        .map_err(ApiError::upstream)
}
