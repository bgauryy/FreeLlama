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
    extract::{Path as AxumPath, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::task::JoinSet;

use crate::{
    model_bench::{Capability, ModelType},
    proxy::{self, ProxyConfig},
    recommend::{
        InstallPlan, InstallationPlanRequest, RecommendationCatalog, installation_plans,
        load_catalog,
    },
};

mod admission;
mod context;
mod discovery;
mod error;
mod execution;
mod footprint;
mod intent;
mod monitor;
mod ollama_env;
mod readiness;
pub mod resources;
mod routing;
mod runtime;
mod telemetry;

pub use discovery::{
    MachineProfile, host_has_unified_memory, host_total_memory_bytes, machine_profile,
};
pub use intent::{RouteIntent, intent_schema, normalize_route_intent, parse_route_intent};
pub use routing::{
    CatalogModel, ExecutionPreference, Objective, PlacementEvidence, RouteDecision, RouteEvidence,
    RouteInput, SessionAffinity, TaskKind, TaskPriority, select_route,
};
pub use runtime::{AdaptiveMode, ContextMode, RuntimeFile};
pub use telemetry::Telemetry;

use admission::{AdmissionPool, MAX_CAPACITY_BYPASSES, PRIORITY_WEIGHTS, priority_index};
use error::{ApiError, resource_error};
pub use execution::runtime_metrics;
use execution::{
    admit, apply_execution_options, execution_target, intent_memory_requirement,
    physical_placement_observation, run_task, select_managed_route, task_cost, transition_timeout,
};
#[cfg(test)]
use execution::{feedback_work_unit_ns, memory_kv_preflight_with_memory, upstream_is_loopback};

// Private helpers the server plane reuses from the pure routing/intent/discovery modules.
use discovery::{load_benchmark, load_policies, parse_capability};
use intent::intent_system_prompt;
use routing::{requested_context, requirements};

const API_ROOT: &str = "/_freellama/v1";
type CatalogCache = Arc<RwLock<Option<(Instant, Vec<CatalogModel>)>>>;

#[derive(Debug, Clone)]
pub struct PlatformConfig {
    /// Shared host pressure policy; clones coordinate CPU/GPU/raw reservations.
    pub resource_governor: resources::ResourceGovernor,
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
    /// Longest a task may wait for admission and a transition lock together before 503. `None` falls
    /// back to `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, then to 120s.
    pub max_queue_wait: Option<Duration>,
    /// Maximum managed requests waiting for the primary/GPU pool. Active requests are counted
    /// separately. `None` falls back to `FREELLAMA_MAX_QUEUED_TASKS`, then to 16.
    pub max_queued_tasks: Option<usize>,
    /// Maximum managed requests waiting for the optional CPU pool. `None` falls back to
    /// `FREELLAMA_CPU_MAX_QUEUED_TASKS`, then to 8.
    pub cpu_max_queued_tasks: Option<usize>,
    /// Maximum live affinity handles. Sessions hold metadata only, never prompt text or Ollama KV.
    pub max_sessions: Option<usize>,
    /// Idle lifetime for affinity handles. Successful route/task use refreshes the TTL.
    pub session_ttl: Option<Duration>,
    /// Generic cap for byte-preserving raw proxy traffic. `None` resolves to one streaming request
    /// in `serve`; the standalone `proxy` command remains opt-in for compatibility.
    pub raw_proxy_max_concurrent_requests: Option<usize>,
    /// How long a raw generation may wait for a slot or for managed execution before 429/503.
    /// `None` falls back to `FREELLAMA_RAW_QUEUE_WAIT_SECONDS`, the runtime file, then 10s.
    pub raw_queue_wait: Option<Duration>,
    /// Optional versioned, atomically replaced adaptive-feedback snapshot.
    pub feedback_file: Option<PathBuf>,
    /// Optional bearer token protecting both control and Ollama-compatible routes.
    pub auth_token: Option<String>,
    /// Explicit opt-in for a non-loopback listener. Requires `auth_token`.
    pub allow_remote: bool,
    /// Optional runtime config file (TOML), re-read when it changes. `None` falls back to
    /// `FREELLAMA_RUNTIME_CONFIG`.
    pub runtime_config: Option<PathBuf>,
    /// Optional JSON-lines usage ledger. `None` falls back to `FREELLAMA_USAGE_FILE`; without
    /// either, usage totals are kept in memory only.
    pub usage_file: Option<PathBuf>,
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
            resource_governor: resources::ResourceGovernor::default(),
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
            max_queued_tasks: None,
            cpu_max_queued_tasks: None,
            max_sessions: None,
            session_ttl: None,
            raw_proxy_max_concurrent_requests: None,
            raw_queue_wait: None,
            feedback_file: None,
            auth_token: None,
            allow_remote: false,
            runtime_config: None,
            usage_file: None,
        }
    }

    /// Read live-tunable settings from `path`; edits are applied without a restart.
    #[must_use]
    pub fn with_runtime_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.runtime_config = Some(path.into());
        self
    }

    /// Append one JSON line per finished managed task to `path` and replay it at startup.
    #[must_use]
    pub fn with_usage_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.usage_file = Some(path.into());
        self
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

    /// Bound retained managed requests while the primary/GPU pool is saturated.
    #[must_use]
    pub fn with_max_queued_tasks(mut self, max: usize) -> Self {
        self.max_queued_tasks = Some(max.max(1));
        self
    }

    /// Bound retained managed requests while the CPU pool is saturated.
    #[must_use]
    pub fn with_cpu_max_queued_tasks(mut self, max: usize) -> Self {
        self.cpu_max_queued_tasks = Some(max.max(1));
        self
    }

    /// Bound in-memory session-affinity metadata for long-lived agents.
    #[must_use]
    pub fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = Some(max_sessions.max(1));
        self
    }

    /// Expire idle session-affinity metadata. This does not evict an Ollama model or its KV cache.
    #[must_use]
    pub fn with_session_ttl(mut self, ttl: Duration) -> Self {
        self.session_ttl = Some(ttl.max(Duration::from_secs(1)));
        self
    }

    #[must_use]
    pub fn with_raw_proxy_max_concurrent_requests(mut self, max: usize) -> Self {
        self.raw_proxy_max_concurrent_requests = Some(max.max(1));
        self
    }

    /// Bound how long raw passthrough generations wait before being refused (0 = refuse at once).
    #[must_use]
    pub fn with_raw_queue_wait(mut self, wait: Duration) -> Self {
        self.raw_queue_wait = Some(wait);
        self
    }

    #[must_use]
    pub fn resolved_raw_proxy_max_concurrent_requests(&self) -> usize {
        self.raw_proxy_max_concurrent_requests
            .unwrap_or(1)
            .clamp(1, Semaphore::MAX_PERMITS)
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
    pub fn resolved_max_queued_tasks(&self) -> usize {
        self.max_queued_tasks
            .unwrap_or_else(max_queued_tasks)
            .max(1)
    }

    #[must_use]
    pub fn resolved_cpu_max_queued_tasks(&self) -> usize {
        self.cpu_max_queued_tasks
            .unwrap_or_else(cpu_max_queued_tasks)
            .max(1)
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
    resources: resources::ResourceGovernor,
    footprints: Arc<Mutex<footprint::FootprintHistory>>,
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
    max_sessions: usize,
    session_ttl: Duration,
    /// Live-tunable limits, waits, eviction, context and breaker settings.
    runtime: runtime::RuntimeSettings,
    telemetry: Telemetry,
    breakers: runtime::Breakers,
    adaptive_gpu: runtime::AdaptiveLimiter,
    adaptive_cpu: runtime::AdaptiveLimiter,
    /// The primary Ollama server's effective configuration, probed at startup.
    ollama: Arc<ollama_env::OllamaSettings>,
    raw_admission: proxy::RawAdmission,
}

impl PlatformState {
    fn tunables(&self) -> runtime::Tunables {
        self.runtime.get()
    }

    fn average_task_ms(&self, placement: &str) -> Option<u64> {
        self.telemetry.average_task_ms(placement)
    }

    fn adaptive_for(&self, placement: &str) -> &runtime::AdaptiveLimiter {
        if placement == "cpu" {
            &self.adaptive_cpu
        } else {
            &self.adaptive_gpu
        }
    }

    /// Push the current tunables into the admission pools and raw admission.
    fn apply_tunables(&self) {
        let tunables = self.tunables();
        self.gpu_admission
            .set_ceiling(tunables.max_concurrent_tasks);
        self.gpu_admission
            .set_max_waiters(tunables.max_queued_tasks);
        self.cpu_admission
            .set_ceiling(tunables.cpu_max_concurrent_tasks);
        self.cpu_admission
            .set_max_waiters(tunables.cpu_max_queued_tasks);
        self.raw_admission.set(
            tunables.raw_max_concurrent_requests,
            tunables.raw_queue_wait(),
        );
    }
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
    gpu_task_cost: usize,
    cpu_task_cost: usize,
    cpu_configured: bool,
}

fn desired_placement(signals: PlacementSignals) -> Option<(&'static str, &'static str)> {
    let gpu_ready = signals.gpu_slots_available >= signals.gpu_task_cost;
    let cpu_ready = signals.cpu_configured && signals.cpu_slots_available >= signals.cpu_task_cost;
    match signals.execution_preference {
        ExecutionPreference::PreferCpu => {
            return Some(if cpu_ready || !gpu_ready {
                ("cpu", "preferred_backend_eligible")
            } else {
                ("gpu", "backend_capacity_available")
            });
        }
        ExecutionPreference::PreferGpu => {
            return Some(if gpu_ready || !cpu_ready {
                ("gpu", "preferred_backend_eligible")
            } else {
                ("cpu", "backend_capacity_available")
            });
        }
        ExecutionPreference::Auto => {}
    }
    if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
        return None;
    }
    match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
        (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
            return Some(if cpu_ready || !gpu_ready {
                ("cpu", "measured_backend_faster")
            } else {
                ("gpu", "backend_capacity_available")
            });
        }
        (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
            return Some(if gpu_ready || !cpu_ready {
                ("gpu", "measured_backend_faster")
            } else {
                ("cpu", "backend_capacity_available")
            });
        }
        _ => {}
    }
    if !gpu_ready && cpu_ready {
        return Some(("cpu", "backend_capacity_available"));
    }
    if !cpu_ready && gpu_ready {
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

/// Build the localhost platform and its Ollama-compatible fallback.
///
/// # Errors
///
/// Returns an error for unsafe configuration, unreadable benchmark evidence, or HTTP setup.
pub fn app(config: &PlatformConfig) -> Result<Router> {
    build(config).map(|(router, _)| router)
}

#[allow(clippy::too_many_lines)] // One linear assembly of state, proxy and routes.
fn build(config: &PlatformConfig) -> Result<(Router, PlatformState)> {
    config.validate()?;
    let benchmark = load_benchmark(config.benchmark_report.as_ref())?;
    let policies = load_policies(config.policy_file.as_ref())?;
    let recommendation_catalog = load_catalog(config.recommendation_catalog.as_ref())?;
    let ollama = ollama_env::probe(&config.upstream);
    let runtime_file = config.runtime_config.clone().or_else(|| {
        std::env::var_os("FREELLAMA_RUNTIME_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let runtime = runtime::RuntimeSettings::new(
        runtime::PinnedTunables {
            max_concurrent_tasks: config.max_concurrent_tasks,
            cpu_max_concurrent_tasks: config.cpu_max_concurrent_tasks,
            max_queued_tasks: config.max_queued_tasks,
            cpu_max_queued_tasks: config.cpu_max_queued_tasks,
            max_queue_wait: config.max_queue_wait,
            raw_max_concurrent_requests: config.raw_proxy_max_concurrent_requests,
            raw_queue_wait: config.raw_queue_wait,
        },
        runtime_file,
        ollama.num_parallel(),
    )?;
    let tunables = runtime.get();
    let usage_file = config.usage_file.clone().or_else(|| {
        std::env::var_os("FREELLAMA_USAGE_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let telemetry = Telemetry::new(usage_file);
    let raw_admission = proxy::RawAdmission::new(
        tunables.raw_max_concurrent_requests,
        tunables.raw_queue_wait(),
    );
    let feedback = config
        .feedback_file
        .as_deref()
        .map(load_feedback)
        .transpose()?
        .unwrap_or_default();
    let state = PlatformState {
        resources: config.resource_governor.clone(),
        footprints: Arc::new(Mutex::new(footprint::FootprintHistory::with_hints(
            footprint::RuntimeHints::from_lookup(|name| ollama.lookup(name)),
        ))),
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
        gpu_admission: AdmissionPool::new(tunables.max_concurrent_tasks, tunables.max_queued_tasks),
        cpu_admission: AdmissionPool::new(
            tunables.cpu_max_concurrent_tasks,
            tunables.cpu_max_queued_tasks,
        ),
        feedback: Arc::new(RwLock::new(feedback)),
        feedback_file: config.feedback_file.clone().map(Arc::new),
        feedback_persistence_error: Arc::new(RwLock::new(None)),
        auth_required: config.auth_token.is_some(),
        remote_access: config.allow_remote,
        max_sessions: config.max_sessions.unwrap_or(1024),
        session_ttl: config.session_ttl.unwrap_or(Duration::from_secs(3600)),
        runtime,
        telemetry: telemetry.clone(),
        breakers: runtime::Breakers::default(),
        adaptive_gpu: runtime::AdaptiveLimiter::default(),
        adaptive_cpu: runtime::AdaptiveLimiter::default(),
        ollama: Arc::new(ollama),
        raw_admission: raw_admission.clone(),
    };
    let fallback_config = ProxyConfig::new(&config.listen, &config.upstream, config.allow_remote)
        .with_raw_admission(raw_admission)
        .with_telemetry(telemetry)
        .with_execution_lock(Arc::clone(&state.managed_execution))
        .with_resource_governor(state.resources.clone());
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
        .route(
            &format!("{API_ROOT}/sessions/{{session_id}}/kill"),
            post(kill_session),
        )
        .route(
            &format!("{API_ROOT}/sessions/{{session_id}}"),
            delete(delete_session),
        )
        .route(&format!("{API_ROOT}/tasks"), post(run_task))
        .route(&format!("{API_ROOT}/task-batches"), post(run_task_batch))
        .route(&format!("{API_ROOT}/metrics"), get(monitor::metrics))
        .route(&format!("{API_ROOT}/status"), get(monitor::status))
        .route(&format!("{API_ROOT}/usage"), get(monitor::usage))
        .route(&format!("{API_ROOT}/config"), get(monitor::config))
        .route(
            &format!("{API_ROOT}/config/reload"),
            post(monitor::reload_config),
        )
        .with_state(state.clone());
    let fallback = proxy::app(fallback_config)?;
    let app = platform.merge(fallback);
    let router = if let Some(token) = config.auth_token.as_deref() {
        app.layer(middleware::from_fn_with_state(
            AuthState {
                token: Arc::from(token),
            },
            require_bearer,
        ))
    } else {
        app
    };
    Ok((router, state))
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
    let (app, state) = build(&config)?;
    if state.runtime.file().is_some() {
        // Poll the runtime config's modification time; a changed file is re-resolved and pushed
        // into admission without a restart. A bad edit keeps the last good values.
        let watched = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            loop {
                ticker.tick().await;
                match watched.runtime.reload(false) {
                    Ok(Some(_)) => {
                        watched.apply_tunables();
                        eprintln!("runtime config reloaded");
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("runtime config not applied: {error}"),
                }
            }
        });
    }
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
    let resource_snapshot = state.resources.snapshot().await;
    let feedback = state.feedback.read().await;
    let persistence_error = state.feedback_persistence_error.read().await.clone();
    let active_sessions = {
        let mut sessions = state.sessions.write().await;
        sessions.prune_expired(state.session_ttl);
        sessions.len()
    };
    let gpu_admission = state.gpu_admission.receipt();
    let cpu_admission = state.cpu_admission.receipt();
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
            "task_batches": "independent_only_bounded_priority_fair",
            "memory_kv_preflight": "metadata_f16_estimate_ollama_final_authority",
            "raw_passthrough_admission": "stream_lifetime_bounded",
        },
        "backends": {
            "gpu": {
                "upstream": state.upstream,
                "admission": gpu_admission,
            },
            "cpu": state.cpu_upstream.as_ref().map(|upstream| json!({
                "upstream": upstream,
                "models": state.cpu_models.as_ref(),
                "admission": cpu_admission,
            })),
        },
        "admission": {
            "scope": "per_backend_weighted_units",
            "slots_total": state.gpu_admission.total() + state.cpu_upstream.as_ref().map_or(0, |_| state.cpu_admission.total()),
            "slots_available": state.gpu_admission.available()
                + state.cpu_upstream.as_ref().map_or(0, |_| state.cpu_admission.available()),
            "max_queue_wait_seconds": state.tunables().max_queue_wait().as_secs(),
            "raw_proxy_max_concurrent_requests": state.tunables().raw_max_concurrent_requests,
            "raw_proxy": state.raw_admission.receipt(),
            "costs": {"embedding": "ceil(input_items/4)", "chat": 2, "vision": 4},
            "priority_fairness": {"policy": "weighted_fair_round_robin", "weights": {"interactive": 3, "normal": 2, "background": 1}, "starvation_prevention": "oldest_capacity_reservation", "max_capacity_bypasses": MAX_CAPACITY_BYPASSES},
            "queue_deadline_scope": "admission_resources_and_transition",
            "resources": resource_snapshot,
        },
        "sessions": {
            "scope": "in_memory_affinity_metadata_only",
            "active": active_sessions,
            "max_sessions": state.max_sessions,
            "idle_ttl_seconds": state.session_ttl.as_secs(),
            "stores_prompt_or_kv": false,
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

/// A session is only an affinity handle. Touch it before any expensive discovery or inference so
/// expired/deleted handles fail promptly and cannot be used to grow an unbounded in-memory map.
async fn require_active_session(
    state: &PlatformState,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if state
        .sessions
        .write()
        .await
        .touch(session_id, state.session_ttl)
    {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ))
    }
}

/// One cancellation boundary for every session-scoped model operation, including interpretation.
async fn with_session_cancellation<T>(
    state: &PlatformState,
    session_id: Option<&str>,
    work: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    let cancellation = if let Some(id) = session_id {
        // Register under the same lock as validation: kill cannot slip between them.
        let mut sessions = state.sessions.write().await;
        if !sessions.touch(id, state.session_ttl) {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "session does not exist or has expired",
            ));
        }
        sessions.cancellation(id)
    } else {
        None
    };
    if let Some(mut cancellation) = cancellation {
        tokio::select! {
            biased;
            () = async {
                if cancellation.wait_for(|killed| *killed).await.is_err() {
                    // Ordinary delete/idle expiry releases affinity only, retaining its contract.
                    std::future::pending::<()>().await;
                }
            } => Err(ApiError::session_killed()),
            result = work => result,
        }
    } else {
        work.await
    }
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
    require_active_session(&state, input.session_id.as_deref()).await?;
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    let route_result =
        select_managed_route(&state, &input, &models, &sessions, task_cost(input.task)).await;
    let (installed_route, installed_execution, installed_route_error) = match route_result {
        Ok(managed) => {
            let execution = managed
                .execution_receipt(task_cost(managed.route.task), &state)
                .await;
            (Some(managed.route), Some(execution), None)
        }
        Err(error) => (None, None, Some(error.body.error)),
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
    require_active_session(&state, input.session_id.as_deref()).await?;
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    let managed =
        select_managed_route(&state, &input, &models, &sessions, task_cost(input.task)).await?;
    drop(sessions);
    let mut value = serde_json::to_value(&managed.route).expect("RouteDecision serializes");
    value["execution"] = managed
        .execution_receipt(task_cost(managed.route.task), &state)
        .await;
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
    let session_id = input.session_id.clone();
    with_session_cancellation(
        &state,
        session_id.as_deref(),
        interpret_natural_route(State(state.clone()), Json(input)),
    )
    .await
}

#[allow(clippy::too_many_lines)] // Keep the admission/guard lifetimes visible in one request flow.
async fn interpret_natural_route(
    State(state): State<PlatformState>,
    Json(input): Json<NaturalRouteInput>,
) -> Result<Json<NaturalRouteResponse>, ApiError> {
    let text = input.text.trim();
    if text.is_empty() || text.len() > 16_384 {
        return Err(ApiError::bad_request(
            "text must contain between 1 and 16384 bytes",
        ));
    }
    if context::estimated_text_tokens(text)
        + context::estimated_text_tokens(intent_system_prompt())
        + 96
        + 512
        > 2048
    {
        return Err(ApiError::bad_request(
            "intent text exceeds the interpreter context budget; shorten the routing description",
        ));
    }
    require_active_session(&state, input.session_id.as_deref()).await?;
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
        "truncate": false,
        "shift": false,
        "format": intent_schema(),
        "keep_alive": "2m",
        "options": {"temperature": 0, "seed": 42, "num_predict": 96, "num_ctx": 2048}
    });
    apply_execution_options(&mut intent_request, &intent_target);
    let response = {
        // Intent inference is still a model generation. Admit it on the assigned backend and take
        // the transition lock so it cannot bypass the same capacity/model-load boundary enforced
        // for managed tasks. Use the write side conservatively because residency has not yet been
        // discovered and a cold interpreter call may replace the active runner.
        let queue_deadline = tokio::time::Instant::now() + state.tunables().max_queue_wait();
        let (intent_slot, _, _) = admit(
            &state,
            &intent_target,
            TaskKind::Completion,
            TaskPriority::Interactive,
            1,
            queue_deadline,
        )
        .await?;
        let intent_bytes = tokio::time::timeout_at(
            queue_deadline,
            intent_memory_requirement(&state, &intent_target),
        )
        .await
        .map_err(|_| transition_timeout(&intent_target))?;
        let intent_resource = state
            .resources
            .wait_for_capacity(
                &intent_target.upstream,
                intent_bytes,
                queue_deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await
            .map_err(resource_error)?;
        let intent_transition =
            tokio::time::timeout_at(queue_deadline, intent_target.transition.write())
                .await
                .map_err(|_| transition_timeout(&intent_target))?;
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
        drop(intent_transition);
        drop(intent_resource);
        drop(intent_slot);
        response
    };
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
    let managed = select_managed_route(
        &state,
        &route_input,
        &models,
        &sessions,
        task_cost(route_input.task),
    )
    .await?;
    drop(sessions);
    Ok(Json(NaturalRouteResponse {
        interpreter_model: state.intent_model.clone(),
        interpreter_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        intent,
        guard_adjustments,
        execution: managed
            .execution_receipt(task_cost(managed.route.task), &state)
            .await,
        route: managed.route,
    }))
}

async fn create_session(State(state): State<PlatformState>) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if sessions.len() >= state.max_sessions {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("session limit reached ({})", state.max_sessions),
        ));
    }
    let id = sessions.create();
    Ok(Json(json!({
        "session_id": id,
        "affinity": "model",
        "stores_prompt_or_kv": false,
        "idle_ttl_seconds": state.session_ttl.as_secs(),
    })))
}

async fn delete_session(
    State(state): State<PlatformState>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if sessions.remove(&session_id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ))
    }
}

async fn kill_session(
    State(state): State<PlatformState>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if !sessions.kill(&session_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ));
    }
    Ok(Json(json!({
        "session_id": session_id,
        "killed": true,
        "cancellation": "requested",
        "runner_stop_confirmed": false,
        "model_unloaded": false,
    })))
}

#[derive(Debug, Clone, Deserialize)]
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
    /// Service class affects admission only; it never changes model selection or bypasses a
    /// backend's weighted capacity.
    #[serde(default)]
    priority: TaskPriority,
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
    /// Per-request admission wait in seconds, capped by the server's `max_queue_wait_seconds`.
    #[serde(default)]
    max_wait_seconds: Option<u64>,
}

/// Explicitly independent managed work. Dependencies are intentionally not accepted: a batch is
/// a bounded dispatcher, not a workflow engine, so an agent cannot accidentally run a dependent
/// step before the value it needs exists.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchTaskInput {
    id: String,
    independent: bool,
    task: TaskInput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBatchInput {
    tasks: Vec<BatchTaskInput>,
    max_parallelism: Option<usize>,
}

const MAX_BATCH_TASKS: usize = 64;
const DEFAULT_BATCH_PARALLELISM: usize = 8;

fn next_batch_task(
    pending: &mut Vec<usize>,
    tasks: &[BatchTaskInput],
    credits: &mut [u8; 3],
) -> usize {
    for pass in 0..2 {
        for (class, credit) in credits.iter_mut().enumerate() {
            if *credit == 0 {
                continue;
            }
            if let Some(position) = pending
                .iter()
                .position(|index| priority_index(tasks[*index].task.priority) == class)
            {
                *credit -= 1;
                return pending.remove(position);
            }
        }
        if pass == 0 {
            *credits = PRIORITY_WEIGHTS;
        }
    }
    // `pending` is non-empty and every priority maps to a class, so this is unreachable unless a
    // future enum variant forgets its scheduler mapping.
    pending.remove(0)
}

async fn run_task_batch(
    State(state): State<PlatformState>,
    Json(input): Json<TaskBatchInput>,
) -> Result<Json<Value>, ApiError> {
    if input.tasks.is_empty() || input.tasks.len() > MAX_BATCH_TASKS {
        return Err(ApiError::bad_request(format!(
            "tasks must contain 1 to {MAX_BATCH_TASKS} items"
        )));
    }
    let mut ids = BTreeSet::new();
    for item in &input.tasks {
        if item.id.trim().is_empty() || !ids.insert(item.id.clone()) {
            return Err(ApiError::bad_request(
                "every batch item needs a distinct, non-empty id",
            ));
        }
        if !item.independent {
            return Err(ApiError::bad_request(format!(
                "batch task {} must declare independent=true; dependency scheduling is not supported",
                item.id
            )));
        }
    }
    let max_parallelism = input
        .max_parallelism
        .unwrap_or(DEFAULT_BATCH_PARALLELISM)
        .clamp(1, MAX_BATCH_TASKS)
        .min(input.tasks.len());
    let mut pending = (0..input.tasks.len()).collect::<Vec<_>>();
    let mut credits = PRIORITY_WEIGHTS;
    let mut results = std::iter::repeat_with(|| None)
        .take(input.tasks.len())
        .collect::<Vec<Option<Value>>>();
    let mut running = JoinSet::new();
    // A panicking worker must fail only its own item: keep completed answers instead of
    // discarding the whole batch, which is what `?` on the JoinError used to do.
    let mut in_flight = BTreeMap::new();

    while !pending.is_empty() || !running.is_empty() {
        while running.len() < max_parallelism && !pending.is_empty() {
            let index = next_batch_task(&mut pending, &input.tasks, &mut credits);
            let task = input.tasks[index].task.clone();
            let task_state = state.clone();
            let handle = running.spawn(Box::pin(run_task(State(task_state), Json(task))));
            in_flight.insert(handle.id(), index);
        }
        if let Some(joined) = running.join_next_with_id().await {
            let (task_id, outcome) = match joined {
                Ok((task_id, result)) => (task_id, Ok(result)),
                Err(error) => (error.id(), Err(error)),
            };
            let Some(index) = in_flight.remove(&task_id) else {
                continue;
            };
            let id = input.tasks[index].id.clone();
            results[index] = Some(match outcome {
                Ok(Ok(Json(response))) => json!({ "id": id, "ok": true, "response": response }),
                Ok(Err(error)) => error.into_batch_result(id),
                Err(error) => ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("batch worker failed: {error}"),
                )
                .into_batch_result(id),
            });
        }
    }
    Ok(Json(json!({
        "independent_only": true,
        "max_parallelism": max_parallelism,
        "scheduler": {
            "scope": "batch_dispatch_and_global_managed_admission",
            "policy": "weighted_fair_round_robin",
            "weights": { "interactive": 3, "normal": 2, "background": 1 },
            "note": "priority affects start order and admission fairness, never route eligibility or capacity",
        },
        "results": results.into_iter().flatten().collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OllamaRequestOptions {
    format: Option<Value>,
    think: Option<Value>,
    options: Option<Map<String, Value>>,
    logprobs: Option<bool>,
    top_logprobs: Option<u32>,
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
    let names = entries
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                .context("Ollama model has no name")
                .map_err(ApiError::upstream)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut shows = show_models(state, upstream, &names).await?;
    let mut models = Vec::with_capacity(entries.len());
    for (index, (entry, name)) in entries.iter().zip(names).enumerate() {
        let Some(show) = shows[index].take() else {
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
        let advertised_context = advertised_context_from_show(&show);
        let kv_cache_bytes_per_token_f16 = estimate_kv_cache_bytes_per_token_f16(&show);
        let modelfile_num_ctx = ollama_env::modelfile_num_ctx(&show);
        let running = resident.iter().find(|running| {
            running
                .get("name")
                .or_else(|| running.get("model"))
                .and_then(Value::as_str)
                == Some(name)
        });
        models.push(CatalogModel {
            digest: entry
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: name.to_owned(),
            size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
            capabilities,
            advertised_context,
            kv_cache_bytes_per_token_f16,
            modelfile_num_ctx,
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

/// Text context belongs to the declared architecture, not an auxiliary vision encoder. Legacy
/// metadata without an architecture is usable only when there is one unambiguous context field.
fn advertised_context_from_show(show: &Value) -> Option<u64> {
    let info = show.get("model_info")?.as_object()?;
    let value = if let Some(architecture) = info.get("general.architecture") {
        info.get(&format!("{}.context_length", architecture.as_str()?))?
    } else {
        let mut values = info
            .iter()
            .filter(|(key, _)| key.ends_with(".context_length"));
        let (_, value) = values.next()?;
        if values.next().is_some() {
            return None;
        }
        value
    };
    value.as_u64().filter(|context| *context > 0)
}

/// Estimate unpadded F16 K+V bytes per token for known, uniform full-attention layouts.
/// This is not live allocation or a lower bound across cache precisions. Ollama additionally
/// handles padding, parallel slots, recurrent state, sliding windows and architecture-specific
/// caches. Return `None` for those layouts rather than pretending the dense formula is exact.
fn estimate_kv_cache_bytes_per_token_f16(show: &Value) -> Option<u64> {
    let info = show.get("model_info")?.as_object()?;
    let architecture = info.get("general.architecture")?.as_str()?;
    if !matches!(
        architecture,
        "llama"
            | "qwen2"
            | "qwen3"
            | "qwen3moe"
            | "gemma"
            | "phi2"
            | "phi3"
            | "stablelm"
            | "command-r"
            | "glmocr"
    ) {
        return None;
    }
    let prefix = format!("{architecture}.");
    if info
        .keys()
        .filter_map(|key| key.strip_prefix(&prefix))
        .any(|key| {
            key.starts_with("ssm.")
                || key.contains("sliding_window")
                || key.contains("shared_kv")
                || key.contains("cross_attention")
        })
    {
        return None;
    }
    // Scope every field to the declared text architecture; multimodal metadata can also contain
    // clip.block_count or other matching suffixes which describe a different network.
    let field = |name: &str| info.get(&format!("{prefix}{name}"));
    let positive = |value: &Value| value.as_u64().filter(|value| *value > 0);
    let blocks = positive(field("block_count")?)?;
    let heads = positive(field("attention.head_count")?)?;
    let kv_heads = positive(field("attention.head_count_kv")?)?;
    let dimension = |name: &str| {
        if let Some(value) = field(name) {
            positive(value)
        } else {
            let embedding = positive(field("embedding_length")?)?;
            (embedding % heads == 0).then_some(embedding / heads)
        }
    };
    let key = dimension("attention.key_length")?;
    let value = dimension("attention.value_length")?;
    blocks
        .checked_mul(kv_heads)?
        .checked_mul(key.checked_add(value)?)?
        .checked_mul(2)
}

/// Concurrent `/api/show` lookups for a whole catalog. Sequential lookups made a cold catalog cost
/// one control round trip per installed model; a small bound keeps a large library from flooding
/// Ollama. Results keep the order of `names`.
const SHOW_CONCURRENCY: usize = 4;

async fn show_models(
    state: &PlatformState,
    upstream: &str,
    names: &[&str],
) -> Result<Vec<Option<Value>>, ApiError> {
    let permits = Arc::new(tokio::sync::Semaphore::new(SHOW_CONCURRENCY));
    let mut lookups = tokio::task::JoinSet::new();
    for (index, name) in names.iter().enumerate() {
        let (state, upstream, name) = (state.clone(), upstream.to_owned(), (*name).to_owned());
        let permits = Arc::clone(&permits);
        lookups.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (index, show_model(&state, &upstream, &name).await)
        });
    }
    let mut shows = vec![None; names.len()];
    while let Some(joined) = lookups.join_next().await {
        let (index, show) = joined
            .context("model metadata lookup panicked")
            .map_err(ApiError::upstream)?;
        shows[index] = show?;
    }
    Ok(shows)
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

fn max_queued_tasks() -> usize {
    std::env::var("FREELLAMA_MAX_QUEUED_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(16)
}

fn cpu_max_queued_tasks() -> usize {
    std::env::var("FREELLAMA_CPU_MAX_QUEUED_TASKS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8)
}

/// Upper bound on a managed generation forwarded to Ollama. Overridable via
/// `FREELLAMA_TASK_TIMEOUT_SECONDS` — the same name the CLI and the NAPI layer read, so one
/// setting covers every path that can make a model generate.
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
mod kv_estimate_tests {
    use super::*;

    fn llama_shape() -> Value {
        json!({"model_info": {
            "general.architecture": "llama",
            "llama.block_count": 16,
            "llama.attention.head_count": 16,
            "llama.attention.head_count_kv": 8,
            "llama.embedding_length": 1536,
        }})
    }

    #[test]
    fn kv_estimate_respects_explicit_key_and_value_dimensions() {
        let mut show = llama_shape();
        show["model_info"]["llama.attention.key_length"] = json!(128);
        show["model_info"]["llama.attention.value_length"] = json!(64);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]["llama.attention.value_length"] = json!(128);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
        // Explicit dimensions do not require a divisible embedding width.
        show["model_info"]["llama.embedding_length"] = json!(1537);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
    }

    #[test]
    fn kv_estimate_never_borrows_another_architectures_fields() {
        let mut show = llama_shape();
        show["model_info"]["clip.block_count"] = json!(999);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]
            .as_object_mut()
            .unwrap()
            .remove("llama.block_count");
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None);
    }

    #[test]
    fn kv_estimate_declares_nonuniform_or_invalid_shapes_unknown() {
        for (key, value) in [
            ("llama.attention.sliding_window", json!(512)),
            ("llama.attention.shared_kv_layers", json!(18)),
            ("llama.ssm.state_size", json!(16)),
            ("llama.attention.head_count_kv", json!([8, 0])),
            ("llama.attention.key_length", json!("128")),
            ("llama.attention.key_length", json!(0)),
            ("llama.block_count", json!(0)),
            ("llama.block_count", json!(u64::MAX)),
            ("general.architecture", json!("new_unknown_family")),
        ] {
            let mut show = llama_shape();
            show["model_info"][key] = value;
            assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None, "{key}");
        }
    }

    #[test]
    fn kv_preflight_does_not_call_f16_a_known_runtime_floor() {
        let model = CatalogModel {
            digest: None,
            name: "test".into(),
            size: 600,
            capabilities: BTreeSet::new(),
            advertised_context: None,
            kv_cache_bytes_per_token_f16: Some(10),
            modelfile_num_ctx: None,
            resident: false,
            resident_vram: None,
            benchmark: BTreeMap::new(),
            policy_rank: BTreeMap::new(),
        };
        let report = memory_kv_preflight_with_memory(&model, Some(30), true, Some(1000));
        assert_eq!(report["refuses"], false);
        assert_eq!(report["status"], "f16_estimate_exceeds_host_budget");
        assert_eq!(report["model_plus_kv_bytes_f16_estimate"], 900);
        assert!(report.get("known_runtime_floor_bytes").is_none());
        let mut large = model.clone();
        large.size = 900;
        large.kv_cache_bytes_per_token_f16 = None;
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), true, Some(1000))["refuses"],
            true
        );
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), false, Some(1000))["refuses"],
            false
        );
    }

    #[test]
    fn kv_preflight_only_compares_loopback_backends_with_local_host_ram() {
        for upstream in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://[::1]:11434",
        ] {
            assert!(upstream_is_loopback(upstream), "{upstream}");
        }
        for upstream in [
            "https://ollama.example.org",
            "http://192.168.1.10:11434",
            "bad-url",
        ] {
            assert!(!upstream_is_loopback(upstream), "{upstream}");
        }
    }
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
                            for gpu_slots_available in [0, 1, 4] {
                                for cpu_slots_available in [0, 1, 4] {
                                    for gpu_task_cost in [1, 2, 4] {
                                        for cpu_task_cost in [1, 2, 4] {
                                            for cpu_configured in [false, true] {
                                                let signals = PlacementSignals {
                                                    route_is_pinned,
                                                    objective,
                                                    execution_preference,
                                                    gpu_work_unit_ns,
                                                    cpu_work_unit_ns,
                                                    gpu_slots_available,
                                                    cpu_slots_available,
                                                    gpu_task_cost,
                                                    cpu_task_cost,
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
            }
        }

        assert_eq!(checked, 2 * 3 * 3 * 2 * 6 * 3 * 3 * 3 * 3 * 2);
    }

    fn placement_oracle(signals: PlacementSignals) -> Option<(&'static str, &'static str)> {
        let gpu_ready = signals.gpu_slots_available >= signals.gpu_task_cost;
        let cpu_ready =
            signals.cpu_configured && signals.cpu_slots_available >= signals.cpu_task_cost;
        match signals.execution_preference {
            ExecutionPreference::PreferCpu => {
                return Some(if cpu_ready || !gpu_ready {
                    ("cpu", "preferred_backend_eligible")
                } else {
                    ("gpu", "backend_capacity_available")
                });
            }
            ExecutionPreference::PreferGpu => {
                return Some(if gpu_ready || !cpu_ready {
                    ("gpu", "preferred_backend_eligible")
                } else {
                    ("cpu", "backend_capacity_available")
                });
            }
            ExecutionPreference::Auto => {}
        }
        if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
            return None;
        }
        match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
            (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
                return Some(if cpu_ready || !gpu_ready {
                    ("cpu", "measured_backend_faster")
                } else {
                    ("gpu", "backend_capacity_available")
                });
            }
            (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
                return Some(if gpu_ready || !cpu_ready {
                    ("gpu", "measured_backend_faster")
                } else {
                    ("cpu", "backend_capacity_available")
                });
            }
            _ => {}
        }
        if !gpu_ready && cpu_ready {
            return Some(("cpu", "backend_capacity_available"));
        }
        if !cpu_ready && gpu_ready {
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
