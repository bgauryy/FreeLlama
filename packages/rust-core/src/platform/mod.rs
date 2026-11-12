//! Machine-aware local-model discovery, routing, sessions, and task execution.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    middleware::{self},
    routing::{delete, get, post},
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

use crate::{
    capability::ModelType,
    proxy::{self, ProxyConfig},
};

mod admission;
mod audit;
mod bearer;
mod budget;
mod catalog;
mod clients;
mod config;
mod context;
mod device_activity;
mod discovery;
mod error;
mod execution;
mod feedback;
mod footprint;
mod job_journal;
mod jobs;
mod model_facts;
mod monitor;
pub(crate) mod ollama_env;
pub mod policy_eval;
mod preload;
mod raw_models;
mod readiness;
mod repetition;
mod residency;
pub mod resources;
mod routing;
mod runtime;
mod scopes;
mod sessions;
mod sweep;
mod tasks_api;
mod telemetry;
mod warming;

use bearer::{AuthState, require_bearer};
use catalog::{discover_models, get_json};
pub use clients::{ClientKey, load_client_keys};
pub use config::PlatformConfig;
use config::{platform_task_timeout, report_admission_budgets};
use feedback::{
    FEEDBACK_SCHEMA_VERSION, MIN_FEEDBACK_IMPROVEMENT_PERCENT, MIN_FEEDBACK_SAMPLES,
    PlacementFeedback, PlacementSignals, desired_placement, load_feedback_or_empty,
    persist_feedback,
};
use sessions::{
    create_session, delete_session, kill_session, require_active_session, with_session_cancellation,
};
use tasks_api::{ManagedOperation, OllamaRequestOptions, TaskInput, run_task_batch};

use discovery::MachineProfile;
pub(crate) use discovery::host_has_unified_memory;
use discovery::host_total_memory_bytes;
pub use discovery::machine_profile;
pub(crate) use discovery::machine_profile_off_runtime;
pub use preload::{PreloadModel, PreloadPolicy, PreloadSpec};
pub use routing::{
    Backend, CatalogModel, ExecutionPreference, LoopRecord, ModelRefusal, ModelRequestDefaults,
    Objective, PlacementEvidence, RouteDecision, RouteInput, SessionAffinity, TaskKind,
    TaskPriority, installed_model, select_route,
};
pub use runtime::RuntimeFile;
pub(crate) use runtime::{ContextMode, ContextShift};
pub(crate) use scopes::ScopePolicy;
pub(crate) use telemetry::Telemetry;
pub(crate) use warming::WarmingPolicy;

use admission::{AdmissionPool, MAX_CAPACITY_BYPASSES};
use error::ApiError;
pub(crate) use execution::runner::PlacementRule;
pub use execution::runtime_metrics;
use execution::{execution_target, physical_placement_observation, run_task, select_managed_route};
#[cfg(test)]
use execution::{feedback_work_unit_ns, memory_kv_preflight_with_memory};

// Private helpers the server plane reuses from the pure routing/discovery modules.
use discovery::{load_benchmark, load_policies};

const API_ROOT: &str = "/_freellama/v1";
type CatalogCache = Arc<RwLock<Option<(Instant, Vec<CatalogModel>)>>>;

/// Lossy `u64` to `f64` for metrics and ratios; values in this crate stay far below 2^52.
#[expect(
    clippy::cast_precision_loss,
    reason = "metrics and ratios; values in this crate stay far below 2^52"
)]
fn float(value: u64) -> f64 {
    value as f64
}

#[derive(Clone)]
struct PlatformState {
    resources: resources::ResourceGovernor,
    footprints: Arc<Mutex<footprint::FootprintHistory>>,
    /// Per-model demand and load history for eviction planning.
    activity: residency::ModelActivity,
    /// Every load, unload and eviction, with its cause and duration.
    audit: audit::Audit,
    /// What each model revision has shown it needs, saved per model in the shared database.
    model_facts: model_facts::ModelFacts,
    /// Startup and predicted loads.
    preload: preload::Preloader,
    /// The most recent eviction plan, for `status` and the UI.
    last_eviction: Arc<std::sync::Mutex<Option<Value>>>,
    client: Client,
    upstream: String,
    cpu_upstream: Option<String>,
    cpu_models: Arc<BTreeSet<String>>,
    benchmark: Arc<discovery::BenchmarkEvidence>,
    policies: Arc<discovery::Policies>,
    sessions: Arc<RwLock<SessionAffinity>>,
    scopes: scopes::ScopeStore,
    catalog_cache: CatalogCache,
    catalog_metadata: catalog::MetadataCache,
    catalog_refresh: Arc<Mutex<()>>,
    /// Per-backend load locks (primary and CPU): they serialize loads and unloads only.
    load_lock: Arc<Mutex<()>>,
    cpu_load_lock: Arc<Mutex<()>>,
    gpu_admission: AdmissionPool,
    cpu_admission: AdmissionPool,
    feedback: Arc<RwLock<PlacementFeedback>>,
    /// The shared database, when this server persists what it learns.
    store: Option<crate::store::Store>,
    feedback_persistence_error: Arc<RwLock<Option<String>>>,
    auth_required: bool,
    /// Client keys, for the usage view of their budgets.
    clients: Arc<[clients::ClientKey]>,
    remote_access: bool,
    max_sessions: usize,
    session_ttl: Duration,
    /// Live-tunable limits, waits, eviction, context and breaker settings.
    runtime: runtime::RuntimeSettings,
    telemetry: Telemetry,
    breakers: runtime::Breakers,
    adaptive_gpu: runtime::AdaptiveLimiter,
    adaptive_cpu: runtime::AdaptiveLimiter,
    /// Cached process observations, refreshed before discovery and monitoring.
    ollama: Arc<RwLock<ollama_env::OllamaSettings>>,
    cpu_ollama: Option<Arc<RwLock<ollama_env::OllamaSettings>>>,
    ollama_refresh: Arc<Mutex<Instant>>,
    raw_admission: proxy::RawAdmission,
    jobs: Arc<std::sync::Mutex<jobs::JobRegistry>>,
    /// Bytes of buffered request bodies, shared with the raw proxy fallback.
    body_budget: proxy::BodyBudget,
    max_request_bytes: usize,
}

impl PlatformState {
    async fn refresh_ollama_settings(&self) {
        let mut refreshed = self.ollama_refresh.lock().await;
        if refreshed.elapsed() < Duration::from_secs(5) {
            return;
        }
        let gpu = self.upstream.clone();
        let cpu = self.cpu_upstream.clone();
        // Process inspection is bounded blocking work; it must not block Tokio's control threads.
        if let Ok((gpu, cpu)) = tokio::task::spawn_blocking(move || {
            (
                ollama_env::probe(&gpu),
                cpu.as_deref().map(ollama_env::probe),
            )
        })
        .await
        {
            let mut footprints = self.footprints.lock().await;
            footprints.set_backend_hints(
                self.upstream.clone(),
                footprint::RuntimeHints::from_lookup(|name| gpu.lookup(name)),
            );
            if let (Some(endpoint), Some(settings)) = (&self.cpu_upstream, &cpu) {
                footprints.set_backend_hints(
                    endpoint.clone(),
                    footprint::RuntimeHints::from_lookup(|name| settings.lookup(name)),
                );
            }
            *self.ollama.write().await = gpu;
            if let (Some(target), Some(settings)) = (&self.cpu_ollama, cpu) {
                *target.write().await = settings;
            }
        }
        *refreshed = Instant::now();
    }

    fn tunables(&self) -> Arc<runtime::Tunables> {
        self.runtime.get()
    }

    fn average_task_ms(&self, placement: Backend) -> Option<u64> {
        self.telemetry.average_task_ms(placement.as_str())
    }

    fn adaptive_for(&self, placement: Backend) -> &runtime::AdaptiveLimiter {
        if placement == Backend::Cpu {
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
        // A reloaded file may add startup preloads; only the new ones load.
        if tokio::runtime::Handle::try_current().is_ok() {
            preload::start(self);
        }
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

#[expect(
    clippy::too_many_lines,
    reason = "one linear assembly of state, proxy and routes"
)]
fn build(config: &PlatformConfig) -> Result<(Router, PlatformState)> {
    config.validate()?;
    let benchmark = load_benchmark(config.benchmark_report.as_ref())?;
    let policies = load_policies(config.policy_file.as_ref())?;
    let ollama = ollama_env::probe(&config.upstream);
    let cpu_ollama = config.cpu_upstream.as_deref().map(ollama_env::probe);
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
    let telemetry =
        Telemetry::with_ledger_capacity(config.store.clone(), tunables.usage_queue_capacity);
    let raw_admission = proxy::RawAdmission::new(
        tunables.raw_max_concurrent_requests,
        tunables.raw_queue_wait(),
    );
    let (feedback, feedback_load_warning) = config
        .store
        .as_ref()
        .map_or_else(Default::default, load_feedback_or_empty);
    let state = PlatformState {
        resources: config.resource_governor.clone(),
        footprints: Arc::new(Mutex::new({
            let mut history = footprint::FootprintHistory::with_hints(
                footprint::RuntimeHints::from_lookup(|name| ollama.lookup(name)),
            );
            if let (Some(upstream), Some(settings)) = (&config.cpu_upstream, &cpu_ollama) {
                history = history.with_backend_hints(
                    upstream.clone(),
                    footprint::RuntimeHints::from_lookup(|name| settings.lookup(name)),
                );
            }
            // Measured runner sizes live in the shared database, so a restart keeps them.
            match &config.store {
                Some(store) => history.with_store(store.clone()),
                None => history,
            }
        })),
        activity: residency::ModelActivity::seeded(config.store.as_ref()),
        audit: audit::Audit::new(config.store.clone()),
        model_facts: model_facts::ModelFacts::loaded(config.store.clone()),
        preload: preload::Preloader::default(),
        last_eviction: Arc::default(),
        // A client-level backstop, not a nicety. A cold load holds the backend load lock across
        // its upstream call, so an untimed request against a wedged Ollama would hold that lock
        // forever and block every subsequent load — one hung request deadlocking the managed
        // plane's cold path. Generous enough
        // for a real generation (Ollama's own OLLAMA_LOAD_TIMEOUT is 5m for the load alone);
        // cheap discovery calls take a much shorter per-request timeout below.
        client: crate::net::build_http_client(|builder| builder.timeout(platform_task_timeout()))
            .context("build platform HTTP client")?,
        upstream: config.upstream.clone(),
        cpu_upstream: config.cpu_upstream.clone(),
        cpu_models: Arc::new(config.cpu_models.clone()),
        benchmark: Arc::new(benchmark),
        policies: Arc::new(policies),
        sessions: Arc::new(RwLock::new(SessionAffinity::default())),
        scopes: scopes::ScopeStore::default(),
        catalog_cache: Arc::new(RwLock::new(None)),
        catalog_metadata: catalog::MetadataCache::default(),
        catalog_refresh: Arc::new(Mutex::new(())),
        load_lock: Arc::new(Mutex::new(())),
        cpu_load_lock: Arc::new(Mutex::new(())),
        gpu_admission: AdmissionPool::new(tunables.max_concurrent_tasks, tunables.max_queued_tasks),
        cpu_admission: AdmissionPool::new(
            tunables.cpu_max_concurrent_tasks,
            tunables.cpu_max_queued_tasks,
        ),
        feedback: Arc::new(RwLock::new(feedback)),
        store: config.store.clone(),
        feedback_persistence_error: Arc::new(RwLock::new(feedback_load_warning)),
        auth_required: config.authenticates(),
        clients: Arc::from(config.client_keys.clone()),
        remote_access: config.allow_remote,
        max_sessions: config.max_sessions.unwrap_or(1024),
        session_ttl: config.session_ttl.unwrap_or(Duration::from_secs(3600)),
        runtime,
        telemetry: telemetry.clone(),
        breakers: runtime::Breakers::default(),
        adaptive_gpu: runtime::AdaptiveLimiter::default(),
        adaptive_cpu: runtime::AdaptiveLimiter::default(),
        ollama: Arc::new(RwLock::new(ollama)),
        cpu_ollama: cpu_ollama.map(|settings| Arc::new(RwLock::new(settings))),
        ollama_refresh: Arc::new(Mutex::new(Instant::now())),
        raw_admission: raw_admission.clone(),
        jobs: Arc::new(std::sync::Mutex::new(jobs::JobRegistry::configured(
            config,
        )?)),
        body_budget: proxy::BodyBudget::shared(),
        max_request_bytes: crate::net::max_request_bytes(),
    };
    let fallback_config = ProxyConfig::new(&config.listen, &config.upstream, config.allow_remote)
        .with_raw_admission(raw_admission)
        .with_telemetry(telemetry)
        .with_load_lock(Arc::clone(&state.load_lock))
        .with_resource_governor(state.resources.clone())
        .with_hooks(Arc::new(raw_models::RawModels(state.clone())))
        .with_upstreams(config.upstreams.clone())
        .with_body_budget(state.body_budget.clone());
    let platform = Router::new()
        .route(&format!("{API_ROOT}/health"), get(health))
        .route(&format!("{API_ROOT}/ready"), get(ready))
        .route(&format!("{API_ROOT}/machine"), get(machine))
        .route(&format!("{API_ROOT}/models"), get(models))
        .route(&format!("{API_ROOT}/routes"), post(route))
        .route(&format!("{API_ROOT}/sessions"), post(create_session))
        .route(
            &format!("{API_ROOT}/sessions/{{session_id}}/kill"),
            post(kill_session),
        )
        .route(
            &format!("{API_ROOT}/sessions/{{session_id}}"),
            delete(delete_session),
        )
        .route(&format!("{API_ROOT}/scopes"), post(scopes::create))
        .route(
            &format!("{API_ROOT}/scopes/{{id}}"),
            get(scopes::get).delete(scopes::delete),
        )
        .route(
            &format!("{API_ROOT}/scopes/{{id}}/fork"),
            post(scopes::fork),
        )
        .route(&format!("{API_ROOT}/warm"), post(warming::warm))
        .route(&format!("{API_ROOT}/unload"), post(warming::unload))
        .route(&format!("{API_ROOT}/tasks"), post(run_task))
        .route(&format!("{API_ROOT}/jobs"), get(jobs::list))
        .route(
            &format!("{API_ROOT}/jobs/{{id}}"),
            get(jobs::get).delete(jobs::remove),
        )
        .route(
            &format!("{API_ROOT}/jobs/{{id}}/cancel"),
            post(jobs::cancel),
        )
        .route(&format!("{API_ROOT}/task-batches"), post(run_task_batch))
        .route(&format!("{API_ROOT}/metrics"), get(monitor::metrics))
        .route(&format!("{API_ROOT}/status"), get(monitor::status))
        .route(&format!("{API_ROOT}/usage"), get(monitor::usage))
        .route(&format!("{API_ROOT}/config"), get(monitor::config))
        .route(
            &format!("{API_ROOT}/config/reload"),
            post(monitor::reload_config),
        )
        // Images travel base64 inside task JSON; axum's own 2 MB default refused a screenshot.
        .layer(axum::extract::DefaultBodyLimit::max(
            state.max_request_bytes,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            reserve_request_body,
        ))
        .with_state(state.clone());
    let fallback = proxy::app(fallback_config)?;
    let app = platform.merge(fallback);
    let router = if config.authenticates() {
        app.layer(middleware::from_fn_with_state(
            AuthState {
                token: config.auth_token.as_deref().map(Arc::from),
                clients: Arc::clone(&state.clients),
                telemetry: state.telemetry.clone(),
            },
            require_bearer,
        ))
    } else {
        app
    };
    // The status page is a static shell with no data in it, so it is served outside the bearer
    // check; its requests to `/status` carry the token the viewer enters.
    let router = router.route("/_freellama/ui", get(monitor::ui));
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
    // The port is taken first so a second server fails fast; until `build` returns, connections
    // wait unanswered, so say what is happening and how long it took.
    eprintln!(
        "freellama: preparing on {} (store, Ollama settings probe, usage replay)",
        config.listen
    );
    let preparing = Instant::now();
    // `build` runs bounded blocking probes (macOS `ps`/`launchctl`); keep them off a runtime
    // worker when the runtime has other workers to hand its tasks to.
    let built = if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(|| build(&config))
    } else {
        build(&config)
    };
    let (app, state) = built?;
    eprintln!(
        "freellama: prepared in {} ms",
        preparing.elapsed().as_millis()
    );
    if state.runtime.file().is_some() {
        // Poll the runtime config's modification time; a changed file is re-resolved and pushed
        // into admission without a restart. A bad edit keeps the last good values.
        let watched = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            loop {
                ticker.tick().await;
                match watched.runtime.reload_off_runtime(false).await {
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
    report_admission_budgets(&state.tunables());
    preload::start(&state);
    sweep::start(&state);
    // A closed stdout must not stop the server; the banner is informational.
    let _ = writeln!(
        std::io::stdout(),
        "FreeLlama platform listening on http://{0}\nStatus page: http://{0}/_freellama/ui",
        config.listen
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::net::shutdown_signal())
        .await
        .context("serve platform")?;
    if !state.telemetry.flush_ledger(Duration::from_secs(2)).await {
        eprintln!("usage ledger flush did not finish within shutdown deadline");
    }
    Ok(())
}

/// Readiness: whether a managed task can be served now, unlike `/health`, which answers as soon
/// as the server listens. Ready means the primary Ollama answers `/api/version` and its model
/// catalog can be read (and the CPU backend answers, when one is configured). A 503 names what
/// is missing, so a caller can wait for it instead of submitting work that would fail.
async fn ready(State(state): State<PlatformState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let unready = |waiting_for: &str, detail: String| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ready": false, "waiting_for": waiting_for, "detail": detail})),
        )
            .into_response()
    };
    let version = match catalog::get_json(&state.client, &state.upstream, "/api/version").await {
        Ok(version) => version,
        Err(error) => {
            return unready(
                "ollama",
                format!(
                    "Ollama at {} does not answer: {}",
                    state.upstream, error.body.error
                ),
            );
        }
    };
    let cpu = if let Some(upstream) = &state.cpu_upstream {
        match catalog::get_json(&state.client, upstream, "/api/version").await {
            Ok(cpu_version) => json!({"reachable": true, "version": cpu_version["version"]}),
            Err(error) => {
                return unready(
                    "cpu_backend",
                    format!(
                        "CPU backend at {upstream} does not answer: {}",
                        error.body.error
                    ),
                );
            }
        }
    } else {
        Value::Null
    };
    let models = match catalog::discover_models(&state).await {
        Ok(models) => models,
        Err(error) => {
            return unready(
                "catalog",
                format!("model catalog unavailable: {}", error.body.error),
            );
        }
    };
    let resident = models.iter().filter(|model| model.resident).count();
    Json(json!({
        "ready": true,
        "ollama": {"reachable": true, "version": version["version"]},
        "cpu_backend": cpu,
        "models": {"installed": models.len(), "resident": resident},
    }))
    .into_response()
}

/// Liveness plus a load-shedding signal.
///
/// An orchestrating agent deciding "delegate, queue, or do it myself" needs a cheap read-only
/// answer to "will a task be admitted right now?". Without it the only way to find out is to
/// submit and possibly eat a 120s queue wait or a 503. `slots_available` is a snapshot — racy by
/// nature, advisory by design — but 0 here means "expect to queue", which is exactly the decision
/// input a caller needs. Standard readiness-endpoint practice, kept on `/health` rather than a new
/// tool so the surface does not grow.
/// Reserve a control-API request's body in the shared buffered-body budget for as long as its
/// handler runs (the parsed task keeps the payload in memory until it finishes). A body over the
/// request limit is not reserved: its extractor refuses it with 413 without reading it.
async fn reserve_request_body(
    State(state): State<PlatformState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    use http_body::Body as _;
    let hint = request.body().size_hint();
    let oversized =
        usize::try_from(hint.lower()).map_or(true, |bytes| bytes > state.max_request_bytes);
    let expected = if oversized {
        0
    } else {
        proxy::expected_body_bytes(&hint, state.max_request_bytes)
    };
    match state
        .body_budget
        .reserve(expected, state.tunables().max_queue_wait())
        .await
    {
        Ok(reservation) => {
            let response = next.run(request).await;
            drop(reservation);
            response
        }
        Err(exhausted) => ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            exhausted.to_string(),
        )
        .with_code("request_body_budget_exhausted")
        .with_retry_after(1)
        .into_response(),
    }
}

async fn health(State(state): State<PlatformState>) -> Json<Value> {
    let resource_snapshot = state.resources.snapshot().await;
    // Lock order: `sessions` is never taken while `feedback` is held. Routing reads `feedback`
    // under a `sessions` snapshot, and tokio's fair RwLock would let a queued feedback writer
    // close the cycle.
    let active_sessions = {
        let mut sessions = state.sessions.write().await;
        sessions.prune_expired(state.session_ttl);
        sessions.len()
    };
    let feedback = state.feedback.read().await;
    let persistence_error = state.feedback_persistence_error.read().await.clone();
    let gpu_admission = state.gpu_admission.receipt();
    let cpu_admission = state.cpu_admission.receipt();
    let feedback_for = |placement: Backend| {
        let values = if placement == Backend::Cpu {
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
                (task.as_str().to_owned(), stats.receipt())
            }).collect::<BTreeMap<_, _>>(),
        })
    };
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        // Contract markers change with routing semantics even when the crate version does not.
        // Tests and agents use them to refuse a serve binary with an older grading contract.
        "contracts": {
            "hardware_fit": "sent_num_ctx",
            "machine_profile": "portable_host_memory_v2",
            "model_backends": "explicit_cpu_assignment",
            "placement_preference": "guarded_hint",
            "placement_observation": "ollama_api_ps_after_execution",
            "placement_evidence_gate": "configured_or_observed",
            "placement_feedback": "three_sample_runtime",
            "placement_feedback_metric": "normalized_work_unit_10_percent",
            "placement_feedback_persistence": "shared_database_snapshot_v2",
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
            "request_body_budget": state.body_budget.receipt(),
            "costs": state.tunables().admission_costs(),
            "priority_fairness": {"policy": "weighted_fair_round_robin", "weights": {"interactive": 3, "normal": 2, "background": 1}, "starvation_prevention": "oldest_capacity_reservation", "max_capacity_bypasses": MAX_CAPACITY_BYPASSES, "model_affinity": {"scope": "within_priority_class", "max_skips": admission::MAX_AFFINITY_SKIPS, "max_wait_ms": u64::try_from(admission::MAX_AFFINITY_WAIT.as_millis()).unwrap_or(u64::MAX)}},
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
                "enabled": state.store.is_some(),
                "schema_version": FEEDBACK_SCHEMA_VERSION,
                "path": state.store.as_ref().and_then(crate::store::Store::path),
                "last_error": persistence_error,
            },
            "gpu": feedback_for(Backend::Gpu),
            "cpu": feedback_for(Backend::Cpu),
        },
    }))
}

async fn machine(State(state): State<PlatformState>) -> Json<MachineProfile> {
    Json(discovery::machine_profile_off_runtime(&state.upstream).await)
}

async fn models(State(state): State<PlatformState>) -> Result<Json<Value>, ApiError> {
    let models = discover_models(&state).await?;
    let mut views = Vec::with_capacity(models.len());
    for model in models {
        {
            let execution = execution_target(&state, &model.name);
            let model_type = ModelType::from_capabilities(model.capabilities.iter().copied());
            // The context Ollama gives a request that leaves `num_ctx` unset: a managed task that
            // sends another value reloads the runner, so callers can see what to match.
            let context_default =
                monitor::ollama_default_context(&state, Some(&model), execution.placement).await;
            let resident_context = model.resident_context;
            let mut observation = physical_placement_observation(
                execution.placement,
                model.resident_size,
                model.resident_vram,
            );
            observation["source"] = json!("ollama_api_ps_catalog");
            let resident_view = catalog::resident_view(
                execution.placement,
                model.resident,
                model.resident_size,
                model.resident_vram,
                &observation,
            );
            let mut value = serde_json::to_value(model).expect("CatalogModel serializes");
            value["model_type"] = json!(model_type);
            value["execution"] = json!({
                "placement": execution.placement,
                "backend": if execution.placement == Backend::Cpu { "cpu" } else { "primary" },
                "upstream": execution.upstream,
                "observation": observation,
            });
            if let Some(view) = resident_view {
                value["resident_view"] = view;
            }
            // Unified-memory hosts leave the tier unknown; a loaded runner still shows the
            // context it was given, which is what a request must match to reuse it.
            value["context_default"] = match (context_default, resident_context) {
                (Some((tokens, source)), _) => json!({"num_ctx": tokens, "source": source}),
                (None, Some(tokens)) => json!({"num_ctx": tokens, "source": "resident_runner"}),
                (None, None) => json!({"num_ctx": Value::Null, "source": "unknown"}),
            };
            views.push(value);
        }
    }
    Ok(Json(json!({"models": views})))
}

async fn route(
    State(state): State<PlatformState>,
    Json(input): Json<RouteInput>,
) -> Result<Json<Value>, ApiError> {
    require_active_session(&state, input.session_id.as_deref()).await?;
    let models = catalog::discover_for_route(&state, input.model.as_deref()).await?;
    // Snapshot the small affinity map so no lock is held across routing's Ollama calls.
    let sessions = state.sessions.read().await.clone();
    let requested_cost = state.tunables().task_cost(input.task);
    let managed =
        select_managed_route(&state, &input, &models, &sessions, requested_cost, None).await?;
    let mut value = serde_json::to_value(&managed.route).expect("RouteDecision serializes");
    value["execution"] = managed.execution_receipt(requested_cost, &state).await;
    Ok(Json(value))
}
