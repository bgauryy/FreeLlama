//! Machine-aware local-model discovery, routing, sessions, and task execution.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    process::Command,
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
use clap::ValueEnum;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    model_bench::{AllModelsReport, Capability},
    proxy::{self, ProxyConfig},
    recommend::{
        InstallPlan, InstallationPlanRequest, RecommendationCatalog, installation_plans,
        load_catalog,
    },
};

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
        }
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

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Completion,
    Coding,
    CodeRepair,
    Tools,
    Browser,
    Vision,
    Embedding,
    LongContext,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    Fastest,
    #[default]
    Balanced,
    Quality,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteInput {
    pub task: TaskKind,
    pub objective: Objective,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub required_capabilities: BTreeSet<Capability>,
    pub context_tokens: Option<u64>,
}

impl Default for RouteInput {
    fn default() -> Self {
        Self {
            task: TaskKind::Completion,
            objective: Objective::Balanced,
            model: None,
            session_id: None,
            required_capabilities: BTreeSet::new(),
            context_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteIntent {
    pub task: TaskKind,
    pub objective: Objective,
    pub context_tokens: Option<u64>,
    pub requires_tools: bool,
    pub requires_vision: bool,
}

impl RouteIntent {
    #[must_use]
    pub fn into_route_input(self, session_id: Option<String>) -> RouteInput {
        let mut required_capabilities = BTreeSet::new();
        if self.requires_tools {
            required_capabilities.insert(Capability::Tools);
        }
        if self.requires_vision {
            required_capabilities.insert(Capability::Vision);
        }
        RouteInput {
            task: self.task,
            objective: self.objective,
            session_id,
            required_capabilities,
            context_tokens: self.context_tokens,
            ..RouteInput::default()
        }
    }
}

/// Return the strict schema used by the local natural-language intent model.
#[must_use]
pub fn intent_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "task": {"type": "string", "enum": ["completion", "coding", "code_repair", "tools", "browser", "vision", "embedding", "long_context"]},
            "objective": {"type": "string", "enum": ["fastest", "balanced", "quality"]},
            "context_tokens": {"type": ["integer", "null"], "minimum": 512},
            "requires_tools": {"type": "boolean"},
            "requires_vision": {"type": "boolean"}
        },
        "required": ["task", "objective", "context_tokens", "requires_tools", "requires_vision"]
    })
}

/// Parse and validate the local intent model's structured output.
///
/// # Errors
///
/// Returns an error for malformed JSON, unknown fields, or invalid enum values.
pub fn parse_route_intent(content: &str) -> Result<RouteIntent> {
    serde_json::from_str(content).context("parse structured route intent")
}

/// Apply deterministic guards for explicit, high-impact natural-language constraints.
#[must_use]
pub fn normalize_route_intent(mut intent: RouteIntent, text: &str) -> (RouteIntent, Vec<String>) {
    let text = text.to_ascii_lowercase();
    let mut adjustments = Vec::new();
    let has_vision_evidence = contains_any(&text, &["screenshot", "image", "photo", "vision"]);
    let has_tool_evidence = contains_any(
        &text,
        &[
            "click",
            "tool call",
            "function call",
            "use tools",
            "search the repository",
            "edit files",
        ],
    );
    let has_context_evidence = contains_any(
        &text,
        &[
            "context",
            "token",
            "large document",
            "long document",
            "long input",
        ],
    );
    if let Some((task, reason)) = explicit_task(&text)
        && intent.task != task
    {
        intent.task = task;
        adjustments.push(reason.to_owned());
    }

    if contains_any(
        &text,
        &[
            "maximum quality",
            "maximum answer quality",
            "best quality",
            "highest quality",
            "best evaluated",
        ],
    ) && intent.objective != Objective::Quality
    {
        intent.objective = Objective::Quality;
        adjustments.push("explicit_quality_objective".to_owned());
    } else if contains_any(
        &text,
        &[
            "fast as possible",
            "fastest",
            "lowest latency",
            "low latency",
        ],
    ) && intent.objective != Objective::Fastest
    {
        intent.objective = Objective::Fastest;
        adjustments.push("explicit_speed_objective".to_owned());
    }
    normalize_inferred_requirements(
        &mut intent,
        has_vision_evidence,
        has_tool_evidence,
        has_context_evidence,
        &mut adjustments,
    );
    (intent, adjustments)
}

fn explicit_task(text: &str) -> Option<(TaskKind, &'static str)> {
    if contains_any(text, &["semantic vector", "embedding", "vector embedding"]) {
        Some((TaskKind::Embedding, "explicit_embedding_term"))
    } else if contains_any(
        text,
        &[
            "fix the bug",
            "bug fix",
            "repair the code",
            "implement the fix",
            "edit files",
        ],
    ) {
        Some((TaskKind::CodeRepair, "explicit_code_repair_term"))
    } else if contains_any(
        text,
        &[
            "browser",
            "webpage",
            "web page",
            "checkout page",
            "click the",
        ],
    ) {
        Some((TaskKind::Browser, "explicit_browser_term"))
    } else if contains_any(
        text,
        &[
            "code review",
            "codebase",
            "debug",
            "rust code",
            "write code",
        ],
    ) {
        Some((TaskKind::Coding, "explicit_coding_term"))
    } else {
        None
    }
}

fn normalize_inferred_requirements(
    intent: &mut RouteIntent,
    has_vision_evidence: bool,
    has_tool_evidence: bool,
    has_context_evidence: bool,
    adjustments: &mut Vec<String>,
) {
    if has_vision_evidence && !intent.requires_vision {
        intent.requires_vision = true;
        adjustments.push("explicit_vision_requirement".to_owned());
    }
    if (matches!(
        intent.task,
        TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair
    ) || has_tool_evidence)
        && !intent.requires_tools
    {
        intent.requires_tools = true;
        adjustments.push("explicit_tool_requirement".to_owned());
    }
    if matches!(intent.task, TaskKind::Embedding) {
        if intent.requires_tools {
            intent.requires_tools = false;
            adjustments.push("embedding_clears_tool_requirement".to_owned());
        }
        if intent.requires_vision {
            intent.requires_vision = false;
            adjustments.push("embedding_clears_vision_requirement".to_owned());
        }
    } else {
        if matches!(intent.task, TaskKind::Vision) && !intent.requires_vision {
            intent.requires_vision = true;
            adjustments.push("vision_task_requires_vision".to_owned());
        } else if !matches!(intent.task, TaskKind::Vision)
            && !has_vision_evidence
            && intent.requires_vision
        {
            intent.requires_vision = false;
            adjustments.push("unsupported_vision_requirement_cleared".to_owned());
        }
        if !matches!(
            intent.task,
            TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair
        ) && !has_tool_evidence
            && intent.requires_tools
        {
            intent.requires_tools = false;
            adjustments.push("unsupported_tool_requirement_cleared".to_owned());
        }
    }
    if intent.context_tokens.is_some() && !has_context_evidence {
        intent.context_tokens = None;
        adjustments.push("unsupported_context_requirement_cleared".to_owned());
    } else if intent.context_tokens.is_some_and(|tokens| tokens < 512) {
        intent.context_tokens = Some(512);
        adjustments.push("context_requirement_clamped".to_owned());
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub name: String,
    pub size: u64,
    pub capabilities: BTreeSet<Capability>,
    pub advertised_context: Option<u64>,
    pub resident: bool,
    pub resident_vram: Option<u64>,
    pub benchmark: BTreeMap<Capability, f64>,
    pub policy_rank: BTreeMap<TaskKind, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    pub selected_model: String,
    pub task: TaskKind,
    pub objective: Objective,
    pub profile: String,
    pub required_capabilities: BTreeSet<Capability>,
    pub options: Value,
    pub think: Value,
    pub stream: bool,
    pub strict_tool_validation: bool,
    pub resident: bool,
    pub session_id: Option<String>,
    pub confidence: String,
    pub evidence: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionAffinity {
    sessions: BTreeMap<String, Option<String>>,
}

impl SessionAffinity {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            sessions: pairs
                .into_iter()
                .map(|(session, model)| (session, Some(model)))
                .collect(),
        }
    }

    fn create(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        self.sessions.insert(id.clone(), None);
        id
    }

    fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    fn assigned(&self, id: &str) -> Option<&str> {
        self.sessions.get(id).and_then(Option::as_deref)
    }

    fn bind(&mut self, id: &str, model: &str) {
        if let Some(slot) = self.sessions.get_mut(id) {
            *slot = Some(model.to_owned());
        }
    }
}

/// Select an eligible local model and a bounded Ollama request profile.
///
/// # Errors
///
/// Returns an error when no installed model satisfies the request contract.
pub fn select_route(
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
) -> Result<RouteDecision> {
    let required = requirements(input);
    let capability_eligible = models
        .iter()
        .filter(|model| required.is_subset(&model.capabilities))
        .filter(|model| {
            input.context_tokens.is_none_or(|requested| {
                model
                    .advertised_context
                    .is_some_and(|available| requested <= available)
            })
        })
        .collect::<Vec<_>>();

    let eligible = if input.model.is_some() || matches!(input.objective, Objective::Fastest) {
        capability_eligible
    } else {
        let qualified = capability_eligible
            .iter()
            .copied()
            .filter(|model| model.policy_rank.contains_key(&input.task))
            .collect::<Vec<_>>();
        ensure!(
            !qualified.is_empty(),
            "no quality-qualified model exists for this task; configure a task policy, choose --objective fastest, or name an explicit model"
        );
        qualified
    };

    let chosen = if let Some(exact) = input.model.as_deref() {
        let installed = models.iter().find(|model| model.name == exact);
        let model = installed.with_context(|| format!("model is not installed: {exact}"))?;
        ensure!(
            eligible.iter().any(|candidate| candidate.name == exact),
            "model is not eligible for the request: {exact}"
        );
        model
    } else if let Some(affine) = input
        .session_id
        .as_deref()
        .and_then(|id| sessions.assigned(id))
        .and_then(|name| eligible.iter().find(|model| model.name == name).copied())
    {
        affine
    } else {
        eligible
            .iter()
            .copied()
            .max_by(|left, right| compare_candidates(left, right, input))
            .context("no installed model is eligible for the request")?
    };

    let (profile, options, think, stream, strict_tool_validation) = profile(input, chosen);
    let capability = ranking_capability(input.task);
    let has_benchmark = chosen.benchmark.contains_key(&capability);
    let policy_qualified = chosen.policy_rank.contains_key(&input.task);
    let mut reasons = vec!["installed".to_owned(), "capabilities_satisfied".to_owned()];
    if input.model.is_some() {
        reasons.push("explicit_model".to_owned());
    } else if input
        .session_id
        .as_deref()
        .and_then(|id| sessions.assigned(id))
        == Some(chosen.name.as_str())
    {
        reasons.push("session_affinity".to_owned());
    } else {
        if chosen.resident {
            reasons.push("resident".to_owned());
        }
        reasons.push(if policy_qualified {
            "configured_task_policy".to_owned()
        } else if has_benchmark {
            "functional_screen_rank".to_owned()
        } else {
            "capability_only_fallback".to_owned()
        });
    }
    let (confidence, evidence) = route_evidence(policy_qualified, has_benchmark);
    Ok(RouteDecision {
        selected_model: chosen.name.clone(),
        task: input.task,
        objective: input.objective,
        profile,
        required_capabilities: required,
        options,
        think,
        stream,
        strict_tool_validation,
        resident: chosen.resident,
        session_id: input.session_id.clone(),
        confidence: confidence.to_owned(),
        evidence: evidence.to_owned(),
        reasons,
    })
}

fn route_evidence(policy_qualified: bool, has_benchmark: bool) -> (&'static str, &'static str) {
    match (policy_qualified, has_benchmark) {
        (true, true) => ("medium", "configured_task_policy"),
        (true, false) => ("low", "configured_task_policy"),
        (false, true) => ("low", "functional_throughput_screen"),
        (false, false) => ("low", "capability_metadata_only"),
    }
}

fn requirements(input: &RouteInput) -> BTreeSet<Capability> {
    let mut required = input.required_capabilities.clone();
    required.insert(ranking_capability(input.task));
    if matches!(
        input.task,
        TaskKind::Tools | TaskKind::Browser | TaskKind::CodeRepair
    ) {
        required.insert(Capability::Completion);
        required.insert(Capability::Tools);
    }
    required
}

fn ranking_capability(task: TaskKind) -> Capability {
    match task {
        TaskKind::Tools | TaskKind::Browser => Capability::Tools,
        TaskKind::Vision => Capability::Vision,
        TaskKind::Embedding => Capability::Embedding,
        TaskKind::Completion | TaskKind::Coding | TaskKind::CodeRepair | TaskKind::LongContext => {
            Capability::Completion
        }
    }
}

fn compare_candidates(left: &CatalogModel, right: &CatalogModel, input: &RouteInput) -> Ordering {
    let capability = ranking_capability(input.task);
    let left_score = left.benchmark.get(&capability).copied();
    let right_score = right.benchmark.get(&capability).copied();
    match input.objective {
        Objective::Fastest => left_score
            .unwrap_or(0.0)
            .total_cmp(&right_score.unwrap_or(0.0))
            .then_with(|| right.size.cmp(&left.size))
            .then_with(|| right.name.cmp(&left.name)),
        Objective::Balanced => left_score
            .is_some()
            .cmp(&right_score.is_some())
            .then_with(|| left.resident.cmp(&right.resident))
            .then_with(|| {
                left_score
                    .unwrap_or(0.0)
                    .total_cmp(&right_score.unwrap_or(0.0))
            })
            .then_with(|| policy_preference(left, right, input.task))
            .then_with(|| right.name.cmp(&left.name)),
        Objective::Quality => {
            policy_preference(left, right, input.task).then_with(|| right.name.cmp(&left.name))
        }
    }
}

fn policy_preference(left: &CatalogModel, right: &CatalogModel, task: TaskKind) -> Ordering {
    let left_rank = left.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    let right_rank = right.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    right_rank.cmp(&left_rank)
}

fn profile(input: &RouteInput, model: &CatalogModel) -> (String, Value, Value, bool, bool) {
    let qwen_repair_profile =
        matches!(input.task, TaskKind::CodeRepair) && model.name == "qwen3.8:27b-mlx";
    let requested_context = if qwen_repair_profile && input.context_tokens.is_none() {
        8_192
    } else {
        requested_context(input)
    };
    let num_ctx = requested_context
        .min(model.advertised_context.unwrap_or(requested_context))
        .min(u64::from(u32::MAX));
    let (name, num_predict, think, strict) = match input.task {
        TaskKind::Browser => ("browser_action", 64, Value::Bool(false), true),
        TaskKind::Tools => ("tools", 256, Value::Bool(false), true),
        TaskKind::CodeRepair if qwen_repair_profile => {
            ("qwen_code_repair", 512, Value::Bool(false), true)
        }
        TaskKind::CodeRepair => ("code_repair", 2_048, thinking_or_off(model, "medium"), true),
        TaskKind::Coding => ("coding", 2_048, thinking_or_off(model, "medium"), false),
        TaskKind::LongContext => (
            "long_context",
            2_048,
            thinking_or_off(model, "medium"),
            false,
        ),
        TaskKind::Vision => ("vision", 512, Value::Bool(false), false),
        TaskKind::Embedding => ("embedding", 0, Value::Null, false),
        TaskKind::Completion => ("completion", 512, Value::Bool(false), false),
    };
    let mut options = json!({"num_ctx": num_ctx});
    if num_predict > 0 {
        options["num_predict"] = json!(num_predict);
    }
    (name.to_owned(), options, think, false, strict)
}

fn requested_context(input: &RouteInput) -> u64 {
    input.context_tokens.unwrap_or(match input.task {
        TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair => 8_192,
        TaskKind::LongContext => 32_768,
        _ => 16_384,
    })
}

fn thinking_or_off(model: &CatalogModel, effort: &str) -> Value {
    if model.capabilities.contains(&Capability::Thinking) {
        json!(effort)
    } else {
        Value::Bool(false)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineProfile {
    pub os: String,
    pub architecture: String,
    pub chip: Option<String>,
    pub logical_cpus: usize,
    pub unified_memory_bytes: Option<u64>,
    pub available_disk_bytes: Option<u64>,
    pub ollama_endpoint: String,
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

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
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

fn intent_system_prompt() -> &'static str {
    "Translate the user's natural-language request into the route-intent schema. Use browser for webpage navigation or interaction; code_repair for fixing a repository bug or editing files to implement a repair; coding for code review, explanation, or diagnosis without a requested repair; tools when function calls are required; vision for image-only analysis; embedding for vectors or semantic search; long_context only for explicitly large documents; otherwise completion. Use fastest only when the user explicitly prioritizes speed or latency, quality only when they explicitly prioritize maximum answer quality, and balanced otherwise. Set requires_tools and requires_vision independently. Never choose or name a model."
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
    if let Some(id) = input.route.session_id.as_deref() {
        state
            .sessions
            .write()
            .await
            .bind(id, &decision.selected_model);
    }

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
    if decision.resident {
        let _permit = state.managed_execution.read().await;
        forward_managed_task(&state, decision, path, body, "resident_shared").await
    } else {
        let _permit = state.managed_execution.write().await;
        forward_managed_task(
            &state,
            decision,
            path,
            body,
            "nonresident_transition_exclusive",
        )
        .await
    }
}

async fn forward_managed_task(
    state: &PlatformState,
    decision: RouteDecision,
    path: &str,
    body: Value,
    admission_mode: &str,
) -> Result<Json<Value>, ApiError> {
    let response = state
        .client
        .post(format!("{}{path}", state.upstream.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .map_err(ApiError::upstream)?;
    let status = response.status();
    let value = response.json::<Value>().await.map_err(ApiError::upstream)?;
    if !status.is_success() {
        return Err(ApiError {
            status,
            message: value.to_string(),
        });
    }
    let metrics = runtime_metrics(&value);
    Ok(Json(json!({
        "route": decision,
        "admission": {"mode": admission_mode},
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
fn platform_task_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("FREELLAMA_TASK_TIMEOUT_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(900),
    )
}

/// Discovery calls (`/api/tags`, `/api/ps`, `/api/show`) read small in-memory state and must never
/// inherit the generation-sized budget above.
fn platform_control_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("FREELLAMA_CONTROL_TIMEOUT_SECONDS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(30),
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

fn load_benchmark(path: Option<&PathBuf>) -> Result<BTreeMap<String, BTreeMap<Capability, f64>>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let bytes =
        std::fs::read(path).with_context(|| format!("read benchmark report {}", path.display()))?;
    let report: AllModelsReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse benchmark report {}", path.display()))?;
    let mut output: BTreeMap<String, BTreeMap<Capability, f64>> = BTreeMap::new();
    for (capability, rankings) in report.rankings {
        for entry in rankings {
            if entry.attempted > 0 && entry.passed == entry.attempted {
                output
                    .entry(entry.model)
                    .or_default()
                    .insert(capability, entry.useful_cases_per_hour);
            }
        }
    }
    Ok(output)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    schema_version: u32,
    #[serde(default)]
    policies: BTreeMap<TaskKind, TaskPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskPolicy {
    qualified_models: Vec<String>,
}

fn load_policies(path: Option<&PathBuf>) -> Result<BTreeMap<TaskKind, Vec<String>>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read policy file {}", path.display()))?;
    let file: PolicyFile =
        toml::from_str(&text).with_context(|| format!("parse policy file {}", path.display()))?;
    ensure!(
        file.schema_version == 1,
        "unsupported policy schema version"
    );
    for (task, policy) in &file.policies {
        ensure!(
            !policy.qualified_models.is_empty(),
            "policy {task:?} has no qualified models"
        );
        ensure!(
            policy
                .qualified_models
                .iter()
                .all(|model| !model.trim().is_empty()),
            "policy {task:?} contains an empty model"
        );
    }
    Ok(file
        .policies
        .into_iter()
        .map(|(task, policy)| (task, policy.qualified_models))
        .collect())
}

fn parse_capability(value: &str) -> Capability {
    match value {
        "completion" => Capability::Completion,
        "tools" => Capability::Tools,
        "vision" => Capability::Vision,
        "audio" => Capability::Audio,
        "thinking" => Capability::Thinking,
        "embedding" => Capability::Embedding,
        _ => Capability::Other,
    }
}

fn machine_profile(upstream: &str) -> MachineProfile {
    MachineProfile {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        chip: command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        unified_memory_bytes: command_output("sysctl", &["-n", "hw.memsize"])
            .and_then(|value| value.parse().ok()),
        available_disk_bytes: command_output("df", &["-Pk", "."])
            .and_then(|value| value.lines().last().map(str::to_owned))
            .and_then(|line| line.split_whitespace().nth(3).map(str::to_owned))
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|kilobytes| kilobytes.checked_mul(1_024)),
        ollama_endpoint: upstream.to_owned(),
    }
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}
