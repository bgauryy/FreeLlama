//! Machine-aware local-model discovery, routing, sessions, and task execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};

use crate::{
    model_bench::{Capability, ModelType},
    proxy::{self, ProxyConfig},
    recommend::{
        InstallPlan, InstallationPlanRequest, RecommendationCatalog, installation_plans,
        load_catalog,
    },
};

mod discovery;
mod intent;
mod routing;

pub use discovery::{MachineProfile, machine_profile};
pub use intent::{RouteIntent, intent_schema, normalize_route_intent, parse_route_intent};
pub use routing::{
    CatalogModel, ExecutionPreference, Objective, PlacementEvidence, RouteDecision, RouteEvidence,
    RouteInput, SessionAffinity, TaskKind, select_route,
};

// Private helpers the server plane reuses from the pure routing/intent/discovery modules.
use discovery::{load_benchmark, load_policies, parse_capability};
use intent::intent_system_prompt;
use routing::{requested_context, requirements};

const API_ROOT: &str = "/_freellama/v1";
type CatalogCache = Arc<RwLock<Option<(Instant, Vec<CatalogModel>)>>>;

#[derive(Debug, Clone)]
pub struct PlatformConfig {
    pub listen: String,
    /// Primary Ollama instance. The byte-preserving fallback proxy always targets this backend.
    pub upstream: String,
    /// Optional second loopback Ollama instance forced to CPU by its own process configuration.
    pub cpu_upstream: Option<String>,
    /// Models whose discovery and managed execution belong to `cpu_upstream`.
    pub cpu_models: BTreeSet<String>,
    pub benchmark_report: Option<PathBuf>,
    pub policy_file: Option<PathBuf>,
    pub recommendation_catalog: Option<PathBuf>,
    pub intent_model: String,
    /// Concurrent managed tasks allowed against Ollama, in cost units. `None` falls back to
    /// `FREELLAMA_MAX_CONCURRENT_TASKS`, then to 2. This is the primary/GPU backend's weighted
    /// admission budget; the legacy field name is retained for compatibility.
    pub max_concurrent_tasks: Option<usize>,
    /// Weighted admission budget for the optional CPU backend. `None` falls back to
    /// `FREELLAMA_CPU_MAX_CONCURRENT_TASKS`, then to 1.
    pub cpu_max_concurrent_tasks: Option<usize>,
    /// Longest a task may queue for an admission slot before being refused with 503. `None` falls
    /// back to `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, then to 120s.
    pub max_queue_wait: Option<Duration>,
    /// Optional versioned, atomically replaced adaptive-feedback snapshot.
    pub feedback_file: Option<PathBuf>,
    /// Optional bearer token protecting both control and Ollama-compatible routes.
    pub auth_token: Option<String>,
    /// Explicit opt-in for a non-loopback listener. Requires `auth_token`.
    pub allow_remote: bool,
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
            cpu_upstream: None,
            cpu_models: BTreeSet::new(),
            benchmark_report,
            policy_file,
            recommendation_catalog: None,
            intent_model: intent_model.into(),
            max_concurrent_tasks: None,
            cpu_max_concurrent_tasks: None,
            max_queue_wait: None,
            feedback_file: None,
            auth_token: None,
            allow_remote: false,
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

    /// Bound weighted work admitted to the primary/GPU Ollama backend.
    ///
    /// Size it for the weighted workload and `OLLAMA_NUM_PARALLEL`: ordinary chat costs two units,
    /// so one parallel chat needs two units. Ollama's own default is 1; extra units keep the pipe
    /// full and bound bursts but do not create within-process decoding concurrency. Exposed on the
    /// config (not env-only) so a test or embedding application can set it without mutating the
    /// process environment.
    #[must_use]
    pub fn with_max_concurrent_tasks(mut self, slots: usize) -> Self {
        self.max_concurrent_tasks = Some(slots.max(1));
        self
    }

    /// Bound weighted work admitted to the optional CPU Ollama backend.
    #[must_use]
    pub fn with_cpu_max_concurrent_tasks(mut self, slots: usize) -> Self {
        self.cpu_max_concurrent_tasks = Some(slots.max(1));
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

    /// Resolve the CPU backend's weighted admission budget.
    #[must_use]
    pub fn resolved_cpu_max_concurrent_tasks(&self) -> usize {
        self.cpu_max_concurrent_tasks
            .unwrap_or_else(cpu_max_concurrent_tasks)
            .clamp(1, Semaphore::MAX_PERMITS)
    }

    #[must_use]
    pub fn with_recommendation_catalog(mut self, path: impl Into<PathBuf>) -> Self {
        self.recommendation_catalog = Some(path.into());
        self
    }

    /// Persist bounded adaptive feedback across service restarts.
    #[must_use]
    pub fn with_feedback_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.feedback_file = Some(path.into());
        self
    }

    /// Require a bearer token on every platform and passthrough request.
    #[must_use]
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Permit a non-loopback listener. Validation still requires bearer authentication.
    #[must_use]
    pub fn with_remote_access(mut self, enabled: bool) -> Self {
        self.allow_remote = enabled;
        self
    }

    /// Route the named models through a second, CPU-configured Ollama server.
    ///
    /// Start `upstream` normally and isolate `cpu_upstream` in a second Ollama process. `FreeLlama`
    /// keeps discovery, residency, transition locking, and managed requests aligned to the assigned
    /// backend, and pins CPU-managed runner loads with `num_gpu: 0`.
    #[must_use]
    pub fn with_cpu_backend<I, S>(mut self, upstream: impl Into<String>, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cpu_upstream = Some(upstream.into());
        self.cpu_models = models.into_iter().map(Into::into).collect();
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
            listen.ip().is_loopback() || (self.allow_remote && self.auth_token.is_some()),
            "non-loopback platform listeners require remote access and bearer authentication"
        );
        if let Some(token) = self.auth_token.as_deref() {
            ensure!(
                token.len() >= 32,
                "authentication token must be at least 32 bytes"
            );
            ensure!(
                token.trim() == token && !token.chars().any(char::is_whitespace),
                "authentication token must not contain whitespace"
            );
        }
        ensure!(
            !self.allow_remote || self.auth_token.is_some(),
            "remote access requires bearer authentication"
        );
        ProxyConfig::new(&self.listen, &self.upstream, self.allow_remote).validate()?;
        if let Some(cpu_upstream) = &self.cpu_upstream {
            ensure!(
                !self.cpu_models.is_empty(),
                "--cpu-upstream requires at least one --cpu-model assignment"
            );
            ensure!(
                !same_ollama_endpoint(&self.upstream, cpu_upstream),
                "CPU and GPU upstreams must be different Ollama instances"
            );
            ProxyConfig::new(&self.listen, cpu_upstream, self.allow_remote).validate()?;
        } else {
            ensure!(
                self.cpu_models.is_empty(),
                "CPU model assignments require a CPU upstream"
            );
        }
        ensure!(
            self.cpu_models.iter().all(|model| !model.trim().is_empty()),
            "CPU model assignments must not be empty"
        );
        ensure!(
            !self.intent_model.trim().is_empty(),
            "intent model must not be empty"
        );
        Ok(())
    }
}

fn same_ollama_endpoint(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    if left.port_or_known_default() != right.port_or_known_default() {
        return false;
    }
    let Some(left_host) = left.host_str().map(normalized_host) else {
        return false;
    };
    let Some(right_host) = right.host_str().map(normalized_host) else {
        return false;
    };
    left_host == right_host || (is_loopback_host(left_host) && is_loopback_host(right_host))
}

fn normalized_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[derive(Clone)]
struct PlatformState {
    client: Client,
    upstream: String,
    cpu_upstream: Option<String>,
    cpu_models: Arc<BTreeSet<String>>,
    benchmark: Arc<BTreeMap<String, BTreeMap<Capability, f64>>>,
    policies: Arc<BTreeMap<TaskKind, Vec<String>>>,
    recommendations: Arc<RecommendationCatalog>,
    sessions: Arc<RwLock<SessionAffinity>>,
    catalog_cache: CatalogCache,
    catalog_refresh: Arc<Mutex<()>>,
    intent_model: String,
    managed_execution: Arc<RwLock<()>>,
    cpu_managed_execution: Arc<RwLock<()>>,
    gpu_admission: AdmissionPool,
    cpu_admission: AdmissionPool,
    feedback: Arc<RwLock<PlacementFeedback>>,
    feedback_file: Option<Arc<PathBuf>>,
    feedback_persistence_error: Arc<RwLock<Option<String>>>,
    auth_required: bool,
    remote_access: bool,
    queue_wait: Duration,
}

#[derive(Clone)]
struct AdmissionPool {
    slots: Arc<Semaphore>,
    total: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FeedbackStats {
    model: Option<String>,
    completed: u64,
    duration_samples: u64,
    total_work_unit_ns: u128,
    total_queue_wait_ms: u128,
    last_work_unit_ns: Option<u64>,
}

impl FeedbackStats {
    fn record(&mut self, model: &str, work_unit_ns: Option<u64>, queue_wait_ms: u128) {
        if self
            .model
            .as_deref()
            .is_some_and(|current| current != model)
        {
            *self = Self::default();
        }
        self.model = Some(model.to_owned());
        self.completed = self.completed.saturating_add(1);
        self.total_queue_wait_ms = self.total_queue_wait_ms.saturating_add(queue_wait_ms);
        if let Some(duration) = work_unit_ns {
            self.duration_samples = self.duration_samples.saturating_add(1);
            self.total_work_unit_ns = self.total_work_unit_ns.saturating_add(u128::from(duration));
            self.last_work_unit_ns = Some(duration);
        }
    }

    fn average_work_unit_ns(&self) -> Option<u128> {
        (self.duration_samples >= MIN_FEEDBACK_SAMPLES && self.total_work_unit_ns > 0)
            .then(|| self.total_work_unit_ns / u128::from(self.duration_samples))
    }

    fn average_for_model(&self, model: &str) -> Option<u128> {
        (self.model.as_deref() == Some(model))
            .then(|| self.average_work_unit_ns())
            .flatten()
    }

    fn receipt(&self) -> Value {
        json!({
            "model": self.model,
            "completed": self.completed,
            "duration_samples": self.duration_samples,
            "decision_ready": self.duration_samples >= MIN_FEEDBACK_SAMPLES && self.total_work_unit_ns > 0,
            "decision_metric": "nanoseconds_per_work_unit",
            "average_work_unit_ns": self.average_work_unit_ns(),
            "average_queue_wait_ms": (self.completed > 0)
                .then(|| self.total_queue_wait_ms / u128::from(self.completed)),
            "last_work_unit_ns": self.last_work_unit_ns,
        })
    }
}

const MIN_FEEDBACK_SAMPLES: u64 = 3;
const MIN_FEEDBACK_IMPROVEMENT_PERCENT: u128 = 10;

fn meaningfully_faster(candidate: u128, baseline: u128) -> bool {
    candidate.saturating_mul(100) < baseline.saturating_mul(100 - MIN_FEEDBACK_IMPROVEMENT_PERCENT)
}

/// The complete, request-local input to the placement hint calculation.
///
/// Keeping this decision independent from HTTP and semaphore ownership makes every combination
/// testable. The selected model is still resolved separately, so a hint can never bypass model
/// eligibility or an operator assignment.
#[derive(Debug, Clone, Copy)]
struct PlacementSignals {
    route_is_pinned: bool,
    objective: Objective,
    execution_preference: ExecutionPreference,
    gpu_work_unit_ns: Option<u128>,
    cpu_work_unit_ns: Option<u128>,
    gpu_slots_available: usize,
    cpu_slots_available: usize,
    cpu_configured: bool,
}

fn desired_placement(signals: PlacementSignals) -> Option<(&'static str, &'static str)> {
    match signals.execution_preference {
        ExecutionPreference::PreferCpu => {
            return Some(("cpu", "preferred_backend_eligible"));
        }
        ExecutionPreference::PreferGpu => {
            return Some(("gpu", "preferred_backend_eligible"));
        }
        ExecutionPreference::Auto => {}
    }
    if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
        return None;
    }
    match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
        (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
            return Some(("cpu", "measured_backend_faster"));
        }
        (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
            return Some(("gpu", "measured_backend_faster"));
        }
        _ => {}
    }
    if signals.cpu_configured && signals.gpu_slots_available == 0 && signals.cpu_slots_available > 0
    {
        return Some(("cpu", "backend_capacity_available"));
    }
    if signals.cpu_configured && signals.cpu_slots_available == 0 && signals.gpu_slots_available > 0
    {
        return Some(("gpu", "backend_capacity_available"));
    }
    None
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PlacementFeedback {
    gpu: BTreeMap<TaskKind, FeedbackStats>,
    cpu: BTreeMap<TaskKind, FeedbackStats>,
}

const FEEDBACK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeedbackSnapshot {
    schema_version: u32,
    feedback: PlacementFeedback,
}

fn load_feedback(path: &Path) -> Result<PlacementFeedback> {
    if !path.exists() {
        return Ok(PlacementFeedback::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read feedback snapshot {}", path.display()))?;
    let snapshot: FeedbackSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse feedback snapshot {}", path.display()))?;
    ensure!(
        snapshot.schema_version == FEEDBACK_SCHEMA_VERSION,
        "unsupported feedback snapshot schema {} in {}",
        snapshot.schema_version,
        path.display()
    );
    Ok(snapshot.feedback)
}

fn persist_feedback(path: &Path, feedback: &PlacementFeedback) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create feedback directory {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create feedback temporary file in {}", parent.display()))?;
    serde_json::to_writer_pretty(
        temporary.as_file_mut(),
        &FeedbackSnapshot {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            feedback: feedback.clone(),
        },
    )
    .context("serialize feedback snapshot")?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync feedback snapshot")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace feedback snapshot {}", path.display()))?;
    Ok(())
}

#[derive(Clone)]
struct AuthState {
    token: Arc<str>,
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn require_bearer(State(auth): State<AuthState>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied.is_some_and(|value| constant_time_eq(value.as_bytes(), auth.token.as_bytes())) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({"error": "missing or invalid bearer token"})),
    )
        .into_response()
}

const fn task_key(task: TaskKind) -> &'static str {
    match task {
        TaskKind::Completion => "completion",
        TaskKind::Coding => "coding",
        TaskKind::CodeRepair => "code_repair",
        TaskKind::Tools => "tools",
        TaskKind::Browser => "browser",
        TaskKind::Vision => "vision",
        TaskKind::Embedding => "embedding",
        TaskKind::LongContext => "long_context",
    }
}

#[derive(Clone)]
struct ExecutionTarget {
    placement: &'static str,
    upstream: String,
    transition: Arc<RwLock<()>>,
    admission: AdmissionPool,
}

fn execution_target(state: &PlatformState, model: &str) -> ExecutionTarget {
    if state.cpu_models.contains(model)
        && let Some(upstream) = &state.cpu_upstream
    {
        return ExecutionTarget {
            placement: "cpu",
            upstream: upstream.clone(),
            transition: Arc::clone(&state.cpu_managed_execution),
            admission: state.cpu_admission.clone(),
        };
    }
    ExecutionTarget {
        placement: "gpu",
        upstream: state.upstream.clone(),
        transition: Arc::clone(&state.managed_execution),
        admission: state.gpu_admission.clone(),
    }
}

struct ManagedDecision {
    route: RouteDecision,
    execution: ExecutionTarget,
    preference: ExecutionPreference,
    preference_satisfied: bool,
    reason: &'static str,
    placement_evidence: PlacementEvidence,
}

impl ManagedDecision {
    fn execution_receipt(&self) -> Value {
        json!({
            // `placement` is retained for compatibility. `backend` names the configured Ollama
            // process; neither field is physical proof. The task response replaces the pending
            // observation below with Ollama's post-run `/api/ps` evidence.
            "placement": self.execution.placement,
            "backend": if self.execution.placement == "cpu" { "cpu" } else { "primary" },
            "requested_processor": self.execution.placement,
            "upstream": self.execution.upstream,
            "preference": self.preference,
            "preference_satisfied": self.preference_satisfied,
            "reason": self.reason,
            "min_placement_evidence": self.placement_evidence,
            "observation": {
                "processor": "unknown",
                "status": "pending",
                "source": "ollama_api_ps_after_execution"
            },
            "admission": {
                "slots_total": self.execution.admission.total,
                "slots_available": self.execution.admission.slots.available_permits(),
            }
        })
    }
}

fn route_candidate_for(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
    placement: &str,
) -> Option<RouteDecision> {
    let candidates = models
        .iter()
        .filter(|model| execution_target(state, &model.name).placement == placement)
        .cloned()
        .collect::<Vec<_>>();
    (!candidates.is_empty())
        .then(|| select_route(input, &candidates, sessions).ok())
        .flatten()
}

fn require_observed_placement(
    input: &RouteInput,
    models: &[CatalogModel],
    route: &RouteDecision,
    execution: &ExecutionTarget,
) -> Result<(), ApiError> {
    if !matches!(input.min_placement_evidence, PlacementEvidence::Observed) {
        return Ok(());
    }
    let selected = models
        .iter()
        .find(|model| model.name == route.selected_model)
        .expect("selected route comes from the supplied catalog");
    let observation = physical_placement_observation(
        execution.placement,
        selected.resident.then_some(selected.size),
        selected.resident_vram,
    );
    if observation["status"] == "verified" {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "physical placement is not verified for {}: configured={}, observed={}. Run one bounded task with min_placement_evidence=configured, inspect execution.observation, then retry with observed",
        route.selected_model,
        execution.placement,
        observation["processor"].as_str().unwrap_or("unknown")
    )))
}

async fn select_managed_route(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
) -> Result<ManagedDecision, ApiError> {
    let has_session_affinity = input
        .session_id
        .as_deref()
        .and_then(|id| sessions.assigned(id))
        .is_some();
    let route_is_pinned = input.model.is_some() || has_session_affinity;
    let gpu_candidate = route_candidate_for(state, input, models, sessions, "gpu");
    let cpu_candidate = route_candidate_for(state, input, models, sessions, "cpu");
    let (gpu_work_unit_ns, cpu_work_unit_ns) = {
        let feedback = state.feedback.read().await;
        (
            gpu_candidate.as_ref().and_then(|route| {
                feedback
                    .gpu
                    .get(&input.task)
                    .and_then(|stats| stats.average_for_model(&route.selected_model))
            }),
            cpu_candidate.as_ref().and_then(|route| {
                feedback
                    .cpu
                    .get(&input.task)
                    .and_then(|stats| stats.average_for_model(&route.selected_model))
            }),
        )
    };
    let desired = desired_placement(PlacementSignals {
        route_is_pinned,
        objective: input.objective,
        execution_preference: input.execution_preference,
        gpu_work_unit_ns,
        cpu_work_unit_ns,
        gpu_slots_available: state.gpu_admission.slots.available_permits(),
        cpu_slots_available: state.cpu_admission.slots.available_permits(),
        cpu_configured: state.cpu_upstream.is_some(),
    });

    let preferred = desired.and_then(|(placement, reason)| {
        if route_is_pinned {
            return None;
        }
        (if placement == "cpu" {
            cpu_candidate.clone()
        } else {
            gpu_candidate.clone()
        })
        .map(|route| (route, reason))
    });
    let (route, mut reason) = if let Some(preferred) = preferred {
        preferred
    } else {
        (
            select_route(input, models, sessions).map_err(ApiError::bad_request)?,
            if desired.is_some() {
                "preferred_backend_unavailable_or_ineligible"
            } else {
                "router_default"
            },
        )
    };
    if input.model.is_some() {
        reason = "explicit_model";
    } else if has_session_affinity {
        reason = "session_affinity";
    }
    let execution = execution_target(state, &route.selected_model);
    require_observed_placement(input, models, &route, &execution)?;
    let preference_satisfied = match input.execution_preference {
        ExecutionPreference::Auto => true,
        ExecutionPreference::PreferCpu => execution.placement == "cpu",
        ExecutionPreference::PreferGpu => execution.placement == "gpu",
    };
    Ok(ManagedDecision {
        route,
        execution,
        preference: input.execution_preference,
        preference_satisfied,
        reason,
        placement_evidence: input.min_placement_evidence,
    })
}

fn normalize_keep_alive(value: Option<String>) -> Value {
    match value {
        // Ollama's API accepts a numeric negative value for infinite residency. Its duration-string
        // parser rejects the superficially equivalent `"-1"` because that string has no unit.
        Some(value) if value == "-1" => json!(-1),
        Some(value) => json!(value),
        None => json!("5m"),
    }
}

fn requests_immediate_unload(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("0" | "0s" | "0m" | "0h"))
}

fn apply_execution_options(body: &mut Value, target: &ExecutionTarget) {
    if target.placement == "cpu" {
        // The process-level CPU library override is ignored by some Metal builds. Ollama's
        // request contract treats num_gpu as a runner load option; pinning zero here makes the
        // explicit CPU assignment real while the second process prevents GPU-runner churn on the
        // primary backend.
        body["options"]["num_gpu"] = json!(0);
    }
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
    let gpu_slots_total = config.resolved_max_concurrent_tasks();
    let cpu_slots_total = config.resolved_cpu_max_concurrent_tasks();
    let feedback = config
        .feedback_file
        .as_deref()
        .map(load_feedback)
        .transpose()?
        .unwrap_or_default();
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
        cpu_upstream: config.cpu_upstream.clone(),
        cpu_models: Arc::new(config.cpu_models.clone()),
        benchmark: Arc::new(benchmark),
        policies: Arc::new(policies),
        recommendations: Arc::new(recommendation_catalog),
        sessions: Arc::new(RwLock::new(SessionAffinity::default())),
        catalog_cache: Arc::new(RwLock::new(None)),
        catalog_refresh: Arc::new(Mutex::new(())),
        intent_model: config.intent_model.clone(),
        managed_execution: Arc::new(RwLock::new(())),
        cpu_managed_execution: Arc::new(RwLock::new(())),
        gpu_admission: AdmissionPool {
            slots: Arc::new(Semaphore::new(gpu_slots_total)),
            total: gpu_slots_total,
        },
        cpu_admission: AdmissionPool {
            slots: Arc::new(Semaphore::new(cpu_slots_total)),
            total: cpu_slots_total,
        },
        feedback: Arc::new(RwLock::new(feedback)),
        feedback_file: config.feedback_file.clone().map(Arc::new),
        feedback_persistence_error: Arc::new(RwLock::new(None)),
        auth_required: config.auth_token.is_some(),
        remote_access: config.allow_remote,
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
    let fallback = proxy::app(ProxyConfig::new(
        &config.listen,
        &config.upstream,
        config.allow_remote,
    ))?;
    let app = platform.merge(fallback);
    Ok(if let Some(token) = config.auth_token.as_deref() {
        app.layer(middleware::from_fn_with_state(
            AuthState {
                token: Arc::from(token),
            },
            require_bearer,
        ))
    } else {
        app
    })
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
    let feedback = state.feedback.read().await;
    let persistence_error = state.feedback_persistence_error.read().await.clone();
    let feedback_for = |placement: &str| {
        let values = if placement == "cpu" {
            &feedback.cpu
        } else {
            &feedback.gpu
        };
        let completed = values.values().map(|stats| stats.completed).sum::<u64>();
        json!({
            "completed": completed,
            "minimum_samples_per_task": MIN_FEEDBACK_SAMPLES,
            "minimum_improvement_percent": MIN_FEEDBACK_IMPROVEMENT_PERCENT,
            "tasks": values.iter().map(|(task, stats)| {
                (task_key(*task).to_owned(), stats.receipt())
            }).collect::<BTreeMap<_, _>>(),
        })
    };
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        // crate version does not change between routing fixes. Tests (and agents) use this to
        // refuse a serve binary that still grades the unclamped chat default as hardware_fit.
        "contracts": {
            "hardware_fit": "sent_num_ctx",
            "machine_profile": "portable_host_memory_v2",
            "model_backends": "explicit_cpu_assignment",
            "placement_preference": "guarded_hint",
            "placement_observation": "ollama_api_ps_after_execution",
            "placement_evidence_gate": "configured_or_observed",
            "placement_feedback": "three_sample_runtime",
            "placement_feedback_metric": "normalized_work_unit_10_percent",
            "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
            "authentication": "optional_bearer_all_routes",
            "immediate_unload_observation": "observe_then_unload",
        },
        "backends": {
            "gpu": {
                "upstream": state.upstream,
                "admission": {
                    "slots_total": state.gpu_admission.total,
                    "slots_available": state.gpu_admission.slots.available_permits(),
                }
            },
            "cpu": state.cpu_upstream.as_ref().map(|upstream| json!({
                "upstream": upstream,
                "models": state.cpu_models.as_ref(),
                "admission": {
                    "slots_total": state.cpu_admission.total,
                    "slots_available": state.cpu_admission.slots.available_permits(),
                }
            })),
        },
        "admission": {
            "scope": "per_backend_weighted_units",
            "slots_total": state.gpu_admission.total + state.cpu_upstream.as_ref().map_or(0, |_| state.cpu_admission.total),
            "slots_available": state.gpu_admission.slots.available_permits()
                + state.cpu_upstream.as_ref().map_or(0, |_| state.cpu_admission.slots.available_permits()),
            "max_queue_wait_seconds": state.queue_wait.as_secs(),
            "costs": {"embedding": 1, "chat": 2, "vision": 4},
        },
        "security": {
            "authentication": if state.auth_required { "bearer" } else { "none" },
            "remote_access": state.remote_access,
            "loopback_unauthenticated": !state.auth_required && !state.remote_access,
        },
        "feedback": {
            "persistence": {
                "enabled": state.feedback_file.is_some(),
                "schema_version": FEEDBACK_SCHEMA_VERSION,
                "path": state.feedback_file.as_deref(),
                "last_error": persistence_error,
            },
            "gpu": feedback_for("gpu"),
            "cpu": feedback_for("cpu"),
        },
    }))
}

async fn machine(State(state): State<PlatformState>) -> Json<MachineProfile> {
    Json(machine_profile(&state.upstream))
}

async fn models(State(state): State<PlatformState>) -> Result<Json<Value>, ApiError> {
    let models = discover_models(&state).await?;
    let models = models
        .into_iter()
        .map(|model| {
            let execution = execution_target(&state, &model.name);
            let model_type = ModelType::from_capabilities(model.capabilities.iter().copied());
            let mut observation = physical_placement_observation(
                execution.placement,
                model.resident.then_some(model.size),
                model.resident_vram,
            );
            observation["source"] = json!("ollama_api_ps_catalog");
            let mut value = serde_json::to_value(model).expect("CatalogModel serializes");
            value["model_type"] = json!(model_type);
            value["execution"] = json!({
                "placement": execution.placement,
                "backend": if execution.placement == "cpu" { "cpu" } else { "primary" },
                "upstream": execution.upstream,
                "observation": observation,
            });
            value
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"models": models})))
}

#[derive(Debug, Serialize)]
pub struct RecommendationResponse {
    pub request: RouteInput,
    pub required_capabilities: BTreeSet<Capability>,
    pub requested_context_tokens: u64,
    pub machine: MachineProfile,
    pub installed_route: Option<RouteDecision>,
    pub installed_execution: Option<Value>,
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
    let route_result = select_managed_route(&state, &input, &models, &sessions).await;
    let (installed_route, installed_execution, installed_route_error) = match route_result {
        Ok(managed) => {
            let execution = managed.execution_receipt();
            (Some(managed.route), Some(execution), None)
        }
        Err(error) => (None, None, Some(error.message)),
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
            memory_bytes: machine.memory_bytes,
            available_disk_bytes: machine.available_disk_bytes,
        },
    );
    Ok(Json(RecommendationResponse {
        request: input,
        required_capabilities,
        requested_context_tokens,
        machine,
        installed_route,
        installed_execution,
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
) -> Result<Json<Value>, ApiError> {
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
    let managed = select_managed_route(&state, &input, &models, &sessions).await?;
    drop(sessions);
    if let Some(id) = input.session_id.as_deref() {
        state
            .sessions
            .write()
            .await
            .bind(id, &managed.route.selected_model);
    }
    let mut value = serde_json::to_value(&managed.route).expect("RouteDecision serializes");
    value["execution"] = managed.execution_receipt();
    Ok(Json(value))
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
    execution: Value,
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
    let intent_target = execution_target(&state, &state.intent_model);
    let mut intent_request = json!({
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
    });
    apply_execution_options(&mut intent_request, &intent_target);
    let response = state
        .client
        .post(format!(
            "{}/api/chat",
            intent_target.upstream.trim_end_matches('/')
        ))
        .timeout(platform_control_timeout().max(Duration::from_secs(120)))
        .json(&intent_request)
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
    let managed = select_managed_route(&state, &route_input, &models, &sessions).await?;
    drop(sessions);
    if let Some(id) = route_input.session_id.as_deref() {
        state
            .sessions
            .write()
            .await
            .bind(id, &managed.route.selected_model);
    }
    Ok(Json(NaturalRouteResponse {
        interpreter_model: state.intent_model,
        interpreter_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        intent,
        guard_adjustments,
        execution: managed.execution_receipt(),
        route: managed.route,
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
    /// Overrides the default `keep_alive` sent to Ollama. `"-1"` is normalized to Ollama's
    /// numeric `-1` infinite-residency form; durations such as `"5m"` and `"0"` pass through. Defaults to
    /// `"5m"` when omitted, matching prior behavior exactly — callers that never set this see no
    /// change. A one-off embedding call is the clearest case for `"0"`: no reason to keep a model
    /// resident after a single vector is computed.
    keep_alive: Option<String>,
    /// Advanced Ollama controls which do not belong to routing. `num_ctx` stays owned by
    /// `context_tokens`, and backend placement owns `num_gpu`, so the route receipt always matches
    /// what is sent upstream.
    #[serde(default)]
    request_options: OllamaRequestOptions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OllamaRequestOptions {
    format: Option<Value>,
    think: Option<Value>,
    options: Option<Map<String, Value>>,
    logprobs: Option<bool>,
    top_logprobs: Option<u32>,
}

fn apply_request_options(
    task: TaskKind,
    decision: &mut RouteDecision,
    request: &OllamaRequestOptions,
) -> Result<(), ApiError> {
    for reserved in ["num_ctx", "num_gpu"] {
        if request
            .options
            .as_ref()
            .is_some_and(|options| options.contains_key(reserved))
        {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "request_options.options.{reserved} is routing-owned; use context_tokens for num_ctx and execution_preference/operator backend assignment for num_gpu"
            )));
        }
    }
    if let Some(format) = &request.format
        && !matches!(format, Value::Object(_))
        && format.as_str() != Some("json")
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.format must be \"json\" or a JSON schema object"
        )));
    }
    if let Some(think) = &request.think
        && !think.is_boolean()
        && !matches!(think.as_str(), Some("low" | "medium" | "high"))
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.think must be a boolean or one of low, medium, high"
        )));
    }
    if request.top_logprobs.is_some() && request.logprobs != Some(true) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.top_logprobs requires logprobs=true"
        )));
    }
    if matches!(task, TaskKind::Embedding)
        && (request.format.is_some()
            || request.think.is_some()
            || request.logprobs.is_some()
            || request.top_logprobs.is_some())
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "embedding tasks accept request_options.options only; format, think, and logprobs are chat controls"
        )));
    }
    let options = decision
        .options
        .as_object_mut()
        .expect("route options are always an object");
    options.extend(request.options.clone().unwrap_or_default());
    if let Some(think) = &request.think {
        decision.think = think.clone();
    }
    Ok(())
}

fn build_managed_request(
    input: &mut TaskInput,
    decision: &RouteDecision,
    keep_alive: &Value,
) -> Result<(&'static str, Value), ApiError> {
    if matches!(input.route.task, TaskKind::Embedding) {
        let value = input
            .input
            .take()
            .context("embedding task requires input")
            .map_err(ApiError::bad_request)?;
        return Ok((
            "/api/embed",
            json!({
                "model": decision.selected_model,
                "input": value,
                "keep_alive": keep_alive,
                "options": decision.options,
            }),
        ));
    }

    let messages = if input.messages.is_empty() {
        let mut message = json!({
            "role": "user",
            "content": input.prompt.take().context("task requires prompt or messages").map_err(ApiError::bad_request)?
        });
        if let Some(images) = input.images.take() {
            message["images"] = json!(images);
        }
        vec![message]
    } else {
        std::mem::take(&mut input.messages)
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
    if let Some(tools) = input.tools.take() {
        body["tools"] = tools;
    }
    if let Some(format) = input.request_options.format.take() {
        body["format"] = format;
    }
    if let Some(logprobs) = input.request_options.logprobs {
        body["logprobs"] = json!(logprobs);
    }
    if let Some(top_logprobs) = input.request_options.top_logprobs {
        body["top_logprobs"] = json!(top_logprobs);
    }
    Ok(("/api/chat", body))
}

/// Wait for an admission slot sized to the task, or refuse.
///
/// Returns the held permit, the cost charged, and how long the caller queued.
async fn admit(
    state: &PlatformState,
    execution: &ExecutionTarget,
    task: TaskKind,
) -> Result<(OwnedSemaphorePermit, u32, u128), ApiError> {
    let queued = Instant::now();
    let budget = u32::try_from(execution.admission.total)
        .unwrap_or(u32::MAX)
        .max(1);
    let cost = task_cost(task).min(budget);
    let wait = state.queue_wait;
    let permit = match tokio::time::timeout(
        wait,
        Arc::clone(&execution.admission.slots).acquire_many_owned(cost),
    )
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
                         units on the {} backend). Retry, or raise {}.",
                    wait.as_secs(),
                    execution.placement,
                    if execution.placement == "cpu" {
                        "--cpu-max-concurrent-tasks"
                    } else {
                        "--max-concurrent-tasks"
                    }
                ),
            });
        }
    };
    Ok((permit, cost, queued.elapsed().as_millis()))
}

async fn run_task(
    State(state): State<PlatformState>,
    Json(mut input): Json<TaskInput>,
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
    let managed = select_managed_route(&state, &input.route, &models, &sessions).await?;
    drop(sessions);

    let execution_receipt = managed.execution_receipt();
    let mut decision = managed.route;
    let execution = managed.execution;
    let immediate_unload = requests_immediate_unload(input.keep_alive.as_deref());
    // `keep_alive:0` makes Ollama unload before its response reaches FreeLlama, so `/api/ps`
    // cannot prove where the work ran. Hold the runner briefly, observe it, then issue and verify
    // an explicit unload before returning. The caller still receives immediate-unload semantics.
    let keep_alive = if immediate_unload {
        json!("30s")
    } else {
        normalize_keep_alive(input.keep_alive.take())
    };
    apply_request_options(input.route.task, &mut decision, &input.request_options)?;
    let (path, mut body) = build_managed_request(&mut input, &decision, &keep_alive)?;
    apply_execution_options(&mut body, &execution);
    // Slot first, THEN the transition lock — in both branches. The order matters: if the
    // non-resident path took the write lock before its slot while resident tasks held slots and
    // waited on the read lock, the two would deadlock. One consistent order removes that entirely.
    let (slot, cost, queue_wait_ms) = admit(&state, &execution, decision.task).await?;

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
        let _permit = execution.transition.read().await;
        forward_managed_task(
            &state,
            decision,
            &execution,
            execution_receipt.clone(),
            path,
            body,
            "resident_shared",
            slot,
            queue_wait_ms,
            cost,
            immediate_unload,
        )
        .await
    } else {
        let _permit = execution.transition.write().await;
        forward_managed_task(
            &state,
            decision,
            &execution,
            execution_receipt,
            path,
            body,
            "nonresident_transition_exclusive",
            slot,
            queue_wait_ms,
            cost,
            immediate_unload,
        )
        .await
    }
}

/// Retry 500/502/504 (load-model blips) but not 503 busy. Same rule as the passthrough proxy.
/// Retrying 503 while holding an admission slot — and on a cold load, the exclusive write lock —
/// amplifies the saturation the semaphore exists to shed.
fn retryable_managed_status(status: StatusCode) -> bool {
    proxy::retryable_upstream_status(status)
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
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let url = format!("{}{path}", upstream.trim_end_matches('/'));
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let more_attempts = attempt < proxy::MAX_ATTEMPTS;
        match state.client.post(&url).json(body).send().await {
            Ok(response) if retryable_managed_status(response.status()) && more_attempts => {
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
    execution: &ExecutionTarget,
    mut execution_receipt: Value,
    path: &str,
    body: Value,
    admission_mode: &str,
    // Held for the duration of the upstream call, then dropped. Taking it by value rather than by
    // reference makes the lifetime the compiler's problem instead of a comment's.
    slot: OwnedSemaphorePermit,
    queue_wait_ms: u128,
    cost: u32,
    immediate_unload: bool,
) -> Result<Json<Value>, ApiError> {
    let (status, value) = post_json_with_retries(state, &execution.upstream, path, &body).await?;
    if !status.is_success() {
        return Err(ApiError {
            status,
            message: value.to_string(),
        });
    }
    let metrics = runtime_metrics(&value);
    let placement = observe_physical_placement(state, execution, &decision.selected_model).await;
    let feedback_accepted = placement["status"] == "verified";
    execution_receipt["observation"] = placement;
    let slots_total = execution.admission.total;
    // Report throttling rather than hiding it. A caller that fans out embeddings needs to know it
    // is queueing here — otherwise the only symptom is latency it cannot attribute.
    let slots_available = execution.admission.slots.available_permits();
    let feedback_receipt = {
        let mut feedback = state.feedback.write().await;
        let by_task = if execution.placement == "cpu" {
            &mut feedback.cpu
        } else {
            &mut feedback.gpu
        };
        let observation = by_task.entry(decision.task).or_default();
        if feedback_accepted {
            let warm_score = if admission_mode == "resident_shared" {
                feedback_work_unit_ns(decision.task, &value)
            } else {
                None
            };
            observation.record(&decision.selected_model, warm_score, queue_wait_ms);
        }
        let mut receipt = observation.receipt();
        receipt["accepted"] = json!(feedback_accepted);
        receipt["reason"] = json!(if feedback_accepted {
            "physical_placement_verified"
        } else {
            "physical_placement_unverified_or_mismatched"
        });
        let snapshot = feedback.clone();
        drop(feedback);
        let persistence = if feedback_accepted {
            if let Some(path) = state.feedback_file.as_deref() {
                match persist_feedback(path, &snapshot) {
                    Ok(()) => {
                        *state.feedback_persistence_error.write().await = None;
                        json!({"enabled": true, "persisted": true, "schema_version": FEEDBACK_SCHEMA_VERSION})
                    }
                    Err(error) => {
                        let message = error.to_string();
                        *state.feedback_persistence_error.write().await = Some(message.clone());
                        json!({"enabled": true, "persisted": false, "error": message})
                    }
                }
            } else {
                json!({"enabled": false, "persisted": false})
            }
        } else {
            json!({"enabled": state.feedback_file.is_some(), "persisted": false, "reason": "sample_not_accepted"})
        };
        receipt["persistence"] = persistence;
        receipt
    };
    if immediate_unload {
        execution_receipt["lifecycle"] =
            unload_after_observation(state, execution, &decision.selected_model).await;
    }
    drop(slot);
    Ok(Json(json!({
        "route": decision,
        "execution": execution_receipt,
        "admission": {
            "mode": admission_mode,
            "queue_wait_ms": queue_wait_ms,
            "slots_total": slots_total,
            "slots_available_during_call": slots_available,
            "cost": cost,
        },
        "metrics": metrics,
        "feedback": feedback_receipt,
        "response": value
    })))
}

async fn unload_after_observation(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
) -> Value {
    let body = json!({"model": model, "keep_alive": 0, "stream": false});
    match post_json_with_retries(state, &execution.upstream, "/api/generate", &body).await {
        Ok((status, _)) if status.is_success() => {
            let observation = observe_physical_placement(state, execution, model).await;
            let unloaded = observation["status"] == "not_resident";
            json!({
                "requested": "immediate_unload",
                "status": if unloaded { "verified" } else { "failed" },
                "post_unload_observation": observation,
            })
        }
        Ok((status, body)) => json!({
            "requested": "immediate_unload",
            "status": "failed",
            "upstream_status": status.as_u16(),
            "error": body,
        }),
        Err(error) => json!({
            "requested": "immediate_unload",
            "status": "failed",
            "error": error.message,
        }),
    }
}

/// Observe the processor that Ollama actually loaded after a managed request. An assignment to
/// the CPU daemon plus `num_gpu:0` is a request, not proof: Metal/MLX builds may still put every
/// byte in VRAM. Unknown/mixed/mismatched observations are returned to the caller and excluded
/// from adaptive feedback so the scheduler cannot learn from a false device label.
async fn observe_physical_placement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
) -> Value {
    let Ok(ps) = get_json(&state.client, &execution.upstream, "/api/ps").await else {
        return json!({
            "processor": "unknown",
            "status": "unavailable",
            "source": "ollama_api_ps_after_execution"
        });
    };
    let running = ps
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|entry| {
                entry
                    .get("name")
                    .or_else(|| entry.get("model"))
                    .and_then(Value::as_str)
                    == Some(model)
            })
        });
    let Some(running) = running else {
        return json!({
            "processor": "unknown",
            "status": "not_resident",
            "source": "ollama_api_ps_after_execution"
        });
    };
    physical_placement_observation(
        execution.placement,
        running.get("size").and_then(Value::as_u64),
        running.get("size_vram").and_then(Value::as_u64),
    )
}

fn physical_placement_observation(
    requested: &str,
    size: Option<u64>,
    size_vram: Option<u64>,
) -> Value {
    let processor = match (size, size_vram) {
        (_, Some(0)) => "cpu",
        (Some(size), Some(vram)) if size > 0 && vram >= size => "gpu",
        (Some(size), Some(vram)) if size > 0 && vram > 0 => "mixed",
        (None, Some(vram)) if vram > 0 => "gpu",
        _ => "unknown",
    };
    let status = if processor == "unknown" {
        "unavailable"
    } else if processor == requested {
        "verified"
    } else {
        "mismatch"
    };
    json!({
        "processor": processor,
        "status": status,
        "source": "ollama_api_ps_after_execution",
        "size": size,
        "size_vram": size_vram,
    })
}

/// Normalize unlike prompt sizes before backend feedback compares them. Generation speed uses
/// decode nanoseconds per output token; embeddings use total nanoseconds per input token because
/// Ollama does not report a separate embedding-evaluation duration.
fn feedback_work_unit_ns(task: TaskKind, response: &Value) -> Option<u64> {
    let (duration, units) = if matches!(task, TaskKind::Embedding) {
        (
            response.get("total_duration").and_then(Value::as_u64),
            response.get("prompt_eval_count").and_then(Value::as_u64),
        )
    } else {
        (
            response.get("eval_duration").and_then(Value::as_u64),
            response.get("eval_count").and_then(Value::as_u64),
        )
    };
    match (duration, units) {
        (Some(duration), Some(units)) if duration > 0 && units > 0 => Some(duration / units),
        _ => None,
    }
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
    if let Some(mut models) = snapshot_catalog(&state.catalog_cache).await {
        refresh_residency(state, &mut models).await?;
        return Ok(models);
    }
    fill_catalog(state).await
}

async fn snapshot_catalog(cache: &CatalogCache) -> Option<Vec<CatalogModel>> {
    cache
        .read()
        .await
        .as_ref()
        .filter(|(saved, _)| saved.elapsed() < Duration::from_secs(30))
        .map(|(_, models)| models.clone())
}

/// Singleflight fill: concurrent cache misses used to each run full tags+ps+per-model show.
async fn fill_catalog(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    let fill = state.catalog_refresh.lock().await;
    if let Some(mut models) = snapshot_catalog(&state.catalog_cache).await {
        drop(fill);
        refresh_residency(state, &mut models).await?;
        return Ok(models);
    }
    let models = fetch_catalog(state).await?;
    *state.catalog_cache.write().await = Some((Instant::now(), models.clone()));
    Ok(models)
}

async fn fetch_catalog(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    let mut models = fetch_catalog_from(state, &state.upstream).await?;
    models.retain(|model| !state.cpu_models.contains(&model.name));
    if let Some(cpu_upstream) = &state.cpu_upstream {
        let mut cpu_models = fetch_catalog_from(state, cpu_upstream).await?;
        cpu_models.retain(|model| state.cpu_models.contains(&model.name));
        models.extend(cpu_models);
    }
    models.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(models)
}

async fn fetch_catalog_from(
    state: &PlatformState,
    upstream: &str,
) -> Result<Vec<CatalogModel>, ApiError> {
    let tags = get_json(&state.client, upstream, "/api/tags").await?;
    let ps = get_json(&state.client, upstream, "/api/ps").await?;
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
        let Some(show) = show_model(state, upstream, name).await? else {
            continue;
        };
        let capabilities = show
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(parse_capability)
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
    Ok(models)
}

/// `None` means skip this tag — a single corrupt `/api/show` must not 502 the whole catalog.
async fn show_model(
    state: &PlatformState,
    upstream: &str,
    name: &str,
) -> Result<Option<Value>, ApiError> {
    match state
        .client
        .post(format!("{}/api/show", upstream.trim_end_matches('/')))
        .timeout(platform_control_timeout())
        .json(&json!({"model": name}))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => Ok(Some(
            response.json::<Value>().await.map_err(ApiError::upstream)?,
        )),
        Ok(response) => {
            eprintln!(
                "skipping model {name}: /api/show returned {}",
                response.status()
            );
            Ok(None)
        }
        Err(error) => {
            eprintln!("skipping model {name}: {error:#}");
            Ok(None)
        }
    }
}

async fn refresh_residency(
    state: &PlatformState,
    models: &mut [CatalogModel],
) -> Result<(), ApiError> {
    let gpu = get_json(&state.client, &state.upstream, "/api/ps").await?;
    let gpu_running = gpu
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cpu_running = if let Some(cpu_upstream) = &state.cpu_upstream {
        get_json(&state.client, cpu_upstream, "/api/ps")
            .await?
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    for model in models {
        let running = if state.cpu_models.contains(&model.name) {
            &cpu_running
        } else {
            &gpu_running
        };
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
/// A flat per-request count is the wrong unit for local inference: embedding, text generation, and
/// image-prefill work are not interchangeable. `FreeLlama` can apply coarse task weights because it
/// knows the task class; Ollama receives an opaque HTTP request and sees memory only after it starts
/// scheduling the runner.
///
/// Deliberately coarse. These are relative costs, not a memory model; Ollama owns the real
/// memory-fit decision (`server/sched.go` evicts when a load is predicted to exceed 80% of free
/// memory) and duplicating that here would mean maintaining a worse copy of it.
fn task_cost(task: TaskKind) -> u32 {
    match task {
        // No autoregressive decode; batching remains the preferred throughput path.
        TaskKind::Embedding => 1,
        // Image payload and multimodal prefill in addition to generation.
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

/// Primary-backend admission budget in weighted units. Default 2 — one ordinary chat generation
/// or two embeddings at `FreeLlama`'s admission layer. CPU has an independent one-unit default.
///
/// Size this for task cost times the desired `OLLAMA_NUM_PARALLEL`. Ollama's own default is **1**,
/// so extra units here do not buy parallel decoding within this backend — they only keep the pipe
/// full and bound the burst. Raising `OLLAMA_NUM_PARALLEL` multiplies KV-cache memory by the context
/// length, so qualify both together and check `models{view:"resident"}` after.
fn max_concurrent_tasks() -> usize {
    std::env::var("FREELLAMA_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2)
}

fn cpu_max_concurrent_tasks() -> usize {
    std::env::var("FREELLAMA_CPU_MAX_CONCURRENT_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
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

#[cfg(test)]
mod feedback_tests {
    use super::{
        ExecutionPreference, FeedbackStats, Objective, PlacementSignals, TaskKind,
        desired_placement, feedback_work_unit_ns, meaningfully_faster,
    };
    use serde_json::json;

    #[test]
    fn backend_feedback_requires_a_real_ten_percent_advantage() {
        assert!(!meaningfully_faster(90, 100));
        assert!(!meaningfully_faster(95, 100));
        assert!(meaningfully_faster(89, 100));
    }

    #[test]
    fn placement_decision_covers_every_signal_permutation() {
        let objectives = [Objective::Fastest, Objective::Balanced, Objective::Quality];
        let preferences = [
            ExecutionPreference::Auto,
            ExecutionPreference::PreferCpu,
            ExecutionPreference::PreferGpu,
        ];
        let gpu_scores = [None, Some(100)];
        let cpu_scores = [None, Some(89), Some(90), Some(100), Some(111), Some(112)];
        let mut checked = 0;

        for route_is_pinned in [false, true] {
            for objective in objectives {
                for execution_preference in preferences {
                    for gpu_work_unit_ns in gpu_scores {
                        for cpu_work_unit_ns in cpu_scores {
                            for gpu_slots_available in [0, 1] {
                                for cpu_slots_available in [0, 1] {
                                    for cpu_configured in [false, true] {
                                        let signals = PlacementSignals {
                                            route_is_pinned,
                                            objective,
                                            execution_preference,
                                            gpu_work_unit_ns,
                                            cpu_work_unit_ns,
                                            gpu_slots_available,
                                            cpu_slots_available,
                                            cpu_configured,
                                        };
                                        let expected = placement_oracle(signals);
                                        assert_eq!(
                                            desired_placement(signals),
                                            expected,
                                            "placement mismatch for {signals:?}"
                                        );
                                        checked += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        assert_eq!(checked, 2 * 3 * 3 * 2 * 6 * 2 * 2 * 2);
    }

    fn placement_oracle(signals: PlacementSignals) -> Option<(&'static str, &'static str)> {
        match signals.execution_preference {
            ExecutionPreference::PreferCpu => {
                return Some(("cpu", "preferred_backend_eligible"));
            }
            ExecutionPreference::PreferGpu => {
                return Some(("gpu", "preferred_backend_eligible"));
            }
            ExecutionPreference::Auto => {}
        }
        if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
            return None;
        }
        match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
            (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
                return Some(("cpu", "measured_backend_faster"));
            }
            (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
                return Some(("gpu", "measured_backend_faster"));
            }
            _ => {}
        }
        if signals.cpu_configured
            && signals.gpu_slots_available == 0
            && signals.cpu_slots_available > 0
        {
            return Some(("cpu", "backend_capacity_available"));
        }
        if signals.cpu_configured
            && signals.cpu_slots_available == 0
            && signals.gpu_slots_available > 0
        {
            return Some(("gpu", "backend_capacity_available"));
        }
        None
    }

    #[test]
    fn feedback_readiness_covers_sample_and_duration_permutations() {
        let mut checked = 0;
        for duration_samples in 0..=4 {
            for total_work_unit_ns in [0, 100] {
                let stats = FeedbackStats {
                    duration_samples,
                    total_work_unit_ns,
                    ..FeedbackStats::default()
                };
                let expected = (duration_samples >= 3 && total_work_unit_ns > 0)
                    .then_some(total_work_unit_ns / u128::from(duration_samples.max(1)));
                assert_eq!(stats.average_work_unit_ns(), expected);
                checked += 1;
            }
        }
        assert_eq!(checked, 10);
    }

    #[test]
    fn feedback_never_combines_different_models_in_one_backend_bucket() {
        let mut stats = FeedbackStats::default();
        for _ in 0..3 {
            stats.record("model-a", Some(100), 4);
        }
        assert_eq!(stats.average_for_model("model-a"), Some(100));
        assert_eq!(stats.average_for_model("model-b"), None);

        stats.record("model-b", Some(50), 2);
        assert_eq!(stats.model.as_deref(), Some("model-b"));
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.duration_samples, 1);
        assert_eq!(stats.average_for_model("model-a"), None);
    }

    #[test]
    fn feedback_metric_uses_the_right_ollama_counters_for_every_task() {
        let response = json!({
            "total_duration": 1_200,
            "prompt_eval_count": 12,
            "eval_duration": 700,
            "eval_count": 7,
        });
        let tasks = [
            TaskKind::Completion,
            TaskKind::Coding,
            TaskKind::CodeRepair,
            TaskKind::Tools,
            TaskKind::Browser,
            TaskKind::Vision,
            TaskKind::Embedding,
            TaskKind::LongContext,
        ];
        for task in tasks {
            assert_eq!(
                feedback_work_unit_ns(task, &response),
                Some(100),
                "wrong normalization counters for {task:?}"
            );
        }
    }
}
