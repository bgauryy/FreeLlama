//! Pure, side-effect-free routing: task/objective types, the model catalog view, session
//! affinity, and the evidence-graded `select_route` decision. No HTTP, no Ollama I/O — this is the
//! part the contract tests drive directly, kept separate so the decision logic can be read and
//! tested without the server plumbing around it.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model_bench::Capability;

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

impl TaskKind {
    /// Wire name, identical to the serde `snake_case` form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completion => "completion",
            Self::Coding => "coding",
            Self::CodeRepair => "code_repair",
            Self::Tools => "tools",
            Self::Browser => "browser",
            Self::Vision => "vision",
            Self::Embedding => "embedding",
            Self::LongContext => "long_context",
        }
    }
}

/// Relative service class for managed work.
///
/// This is deliberately an execution concern, rather than a routing preference: it never changes
/// which model is eligible. Admission uses weighted fair scheduling (interactive 3, normal 2,
/// background 1), so low-priority work makes bounded progress instead of starving behind a
/// continuous interactive stream.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default, ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Interactive,
    #[default]
    Normal,
    Background,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    Fastest,
    #[default]
    Balanced,
    Quality,
}

/// A caller's backend preference. Operators retain authority over which exact models are assigned
/// to the CPU backend; this value only chooses among models that are already eligible there.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPreference {
    #[default]
    Auto,
    PreferCpu,
    PreferGpu,
}

/// The configured Ollama backend a model executes on. Serializes as the `"cpu"`/`"gpu"` wire
/// strings used in placement receipts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cpu,
    Gpu,
}

impl Backend {
    /// The wire string: `"cpu"` or `"gpu"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Minimum proof required before a route may be returned. `Observed` intentionally refuses a
/// cold model: warm it once with configured evidence, inspect the receipt, then require physical
/// evidence for subsequent work where CPU/GPU placement is a hard constraint.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum PlacementEvidence {
    #[default]
    Configured,
    Observed,
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
    /// Prefer an operator-configured backend without forcing an ineligible route.
    pub execution_preference: ExecutionPreference,
    /// Whether an operator assignment is enough, or matching `/api/ps` evidence is required.
    pub min_placement_evidence: PlacementEvidence,
    /// Refuse the route rather than return one graded below this.
    ///
    /// The gate lives in the router so the CLI, the HTTP API, the MCP server and library
    /// embedders all fail closed the same way.
    #[serde(default)]
    pub min_confidence: Option<String>,
}

impl Default for RouteInput {
    fn default() -> Self {
        Self {
            task: TaskKind::Completion,
            objective: Objective::Balanced,
            min_confidence: None,
            model: None,
            session_id: None,
            required_capabilities: BTreeSet::new(),
            context_tokens: None,
            execution_preference: ExecutionPreference::Auto,
            min_placement_evidence: PlacementEvidence::Configured,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub name: String,
    /// Content identity from Ollama's manifest; absent metadata must not be treated as a tag hash.
    #[serde(default)]
    pub digest: Option<String>,
    /// Installed model file bytes reported by `/api/tags`, used for routing and forecasts.
    pub size: u64,
    pub capabilities: BTreeSet<Capability>,
    pub advertised_context: Option<u64>,
    /// F16-equivalent bytes needed for K and V cache per token, derived from Ollama's model
    /// metadata when its architecture fields are available. It is intentionally optional: not all
    /// model families expose a shape `FreeLlama` can calculate without guessing.
    #[serde(default)]
    pub kv_cache_bytes_per_token_f16: Option<u64>,
    /// `PARAMETER num_ctx` from the model's Modelfile. When set, Ollama uses it instead of its
    /// automatic context for requests that leave `num_ctx` unset.
    #[serde(default)]
    pub modelfile_num_ctx: Option<u64>,
    /// `general.architecture` from Ollama's model metadata: the prefix of the model's own
    /// context and attention fields. Routing never ranks by it.
    #[serde(default)]
    pub architecture: Option<String>,
    pub resident: bool,
    /// Loaded runner bytes reported by `/api/ps`, distinct from the installed file size.
    #[serde(default)]
    pub resident_size: Option<u64>,
    pub resident_vram: Option<u64>,
    /// `context_length` of the loaded runner reported by `/api/ps`: the context a request must
    /// match to reuse it (any other explicit `num_ctx` reloads the runner).
    #[serde(default)]
    pub resident_context: Option<u64>,
    pub benchmark: BTreeMap<Capability, f64>,
    /// Managed runs of this model per task kind in this process, and how many of them ended in a
    /// repetition loop. Observed behaviour, not a guess from the model's name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub observed_loops: BTreeMap<TaskKind, LoopRecord>,
    pub policy_rank: BTreeMap<TaskKind, usize>,
    /// Per-task request defaults a policy file sets for this model; routing applies the entry
    /// for the requested task on top of the generic task profile.
    #[serde(default)]
    pub policy_options: BTreeMap<TaskKind, ModelRequestDefaults>,
}

/// How often managed runs of one model on one task kind ended in a repetition loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRecord {
    pub runs: u32,
    pub loops: u32,
}

impl LoopRecord {
    /// At least two loops in at most twice as many runs: a model that keeps repeating itself on
    /// this kind of task, which one stray loop does not make.
    #[must_use]
    pub const fn keeps_looping(self) -> bool {
        self.loops >= 2 && self.loops.saturating_mul(2) >= self.runs
    }
}

/// Request defaults one task policy sets for one model
/// (`[policies.<task>.model_options."<model>"]`). Unset fields keep the generic task profile.
/// Context size is routing-owned (prompt estimate and `context_tokens`), so it is not a policy key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestDefaults {
    #[serde(default)]
    pub think: Option<bool>,
    /// Must be at least 1; the policy loader rejects 0.
    #[serde(default)]
    pub num_predict: Option<u64>,
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
    pub strict_tool_validation: bool,
    pub resident: bool,
    pub session_id: Option<String>,
    pub confidence: String,
    pub evidence: String,
    /// The dimensions `confidence` is derived from, reported separately so routing is inspectable
    /// rather than a single word a caller may mistake for a calibrated probability.
    pub quality_evidence: String,
    pub task_evidence: String,
    /// Whether the sent context fits the model's advertised context window. This is not a RAM,
    /// KV-cache, or live runner-admission claim.
    pub context_window_fit: String,
    /// Deprecated alias; retained for older callers.
    pub hardware_fit: String,
    /// Every other eligible candidate and why it lost.
    pub rejected: Vec<Value>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionAffinity {
    sessions: BTreeMap<String, SessionRecord>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    model: Option<String>,
    last_used: Instant,
    killed: tokio::sync::watch::Sender<bool>,
}

impl SessionAffinity {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            sessions: pairs
                .into_iter()
                .map(|(session, model)| {
                    (
                        session,
                        SessionRecord {
                            model: Some(model),
                            last_used: Instant::now(),
                            killed: tokio::sync::watch::channel(false).0,
                        },
                    )
                })
                .collect(),
        }
    }

    pub(super) fn create(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        self.sessions.insert(
            id.clone(),
            SessionRecord {
                model: None,
                last_used: Instant::now(),
                killed: tokio::sync::watch::channel(false).0,
            },
        );
        id
    }

    pub(super) fn assigned(&self, id: &str) -> Option<&str> {
        self.sessions
            .get(id)
            .and_then(|record| record.model.as_deref())
    }

    pub(super) fn bind(&mut self, id: &str, model: &str) {
        if let Some(record) = self.sessions.get_mut(id) {
            record.model = Some(model.to_owned());
            record.last_used = Instant::now();
        }
    }

    pub(super) fn touch(&mut self, id: &str, ttl: Duration) -> bool {
        self.prune_expired(ttl);
        if let Some(record) = self.sessions.get_mut(id) {
            record.last_used = Instant::now();
            true
        } else {
            false
        }
    }

    pub(super) fn prune_expired(&mut self, ttl: Duration) {
        let now = Instant::now();
        self.sessions
            .retain(|_, record| now.duration_since(record.last_used) < ttl);
    }

    pub(super) fn remove(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    pub(super) fn cancellation(&self, id: &str) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.sessions
            .get(id)
            .map(|record| record.killed.subscribe())
    }

    pub(super) fn kill(&mut self, id: &str) -> bool {
        let Some(record) = self.sessions.remove(id) else {
            return false;
        };
        record.killed.send_replace(true);
        true
    }

    pub(super) fn len(&self) -> usize {
        self.sessions.len()
    }
}

/// Select an eligible local model and a bounded Ollama request profile.
///
/// The installed model a caller's name denotes, the way Ollama reads a name: the exact tag, else
/// `name:latest`, else the one installed tag of that model family. An ambiguous or unknown name
/// is refused with the installed candidates, never substituted.
///
/// # Errors
///
/// Returns an error when the name matches no installed model or more than one tag.
pub fn installed_model<'a>(
    models: &'a [CatalogModel],
    requested: &str,
) -> Result<&'a CatalogModel> {
    if let Some(exact) = models.iter().find(|model| model.name == requested) {
        return Ok(exact);
    }
    if !requested.contains(':') {
        let latest = format!("{requested}:latest");
        if let Some(model) = models.iter().find(|model| model.name == latest) {
            return Ok(model);
        }
        let family = models
            .iter()
            .filter(|model| {
                model
                    .name
                    .split_once(':')
                    .is_some_and(|(name, _)| name == requested)
            })
            .collect::<Vec<_>>();
        match family.as_slice() {
            [only] => return Ok(only),
            [_, ..] => bail!(
                "model name is ambiguous: {requested} (installed: {})",
                family
                    .iter()
                    .map(|model| model.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            [] => {}
        }
    }
    let installed = models
        .iter()
        .map(|model| model.name.as_str())
        .collect::<Vec<_>>();
    bail!(
        "model is not installed: {requested} (installed: {})",
        if installed.is_empty() {
            "none".to_owned()
        } else {
            installed.join(", ")
        }
    )
}

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

    let eligible = eligible_for_objective(input, capability_eligible)?;

    let chosen = if let Some(requested) = input.model.as_deref() {
        let model = installed_model(models, requested)?;
        ensure!(
            eligible
                .iter()
                .any(|candidate| candidate.name == model.name),
            "model is not eligible for the request: {}",
            model.name
        );
        model
    } else if let Some(affine) = input
        .session_id
        .as_deref()
        .and_then(|id| sessions.assigned(id))
        .and_then(|name| eligible.iter().find(|model| model.name == name).copied())
        .filter(|model| general_fallback_fit(model, input.task))
    {
        affine
    } else {
        eligible
            .iter()
            .copied()
            .max_by(|left, right| compare_candidates(left, right, input))
            .context("no installed model is eligible for the request")?
    };

    let TaskProfile {
        name: profile,
        options,
        think,
        strict_tool_validation,
        policy_model_options,
        policy_think_dropped,
    } = profile(input, chosen);
    let capability = ranking_capability(input.task);
    let has_benchmark = chosen.benchmark.contains_key(&capability);
    let policy_qualified = chosen.policy_rank.contains_key(&input.task);
    let mut reasons = vec!["installed".to_owned(), "capabilities_satisfied".to_owned()];
    if input.model.is_none() && !general_fallback_fit(chosen, input.task) {
        reasons.push("repeats_on_this_task_last_resort".to_owned());
    }
    if policy_model_options {
        reasons.push("policy_model_options".to_owned());
    }
    if policy_think_dropped {
        reasons.push("policy_think_ignored_without_thinking_capability".to_owned());
    }
    if input.model.is_none() && matches!(input.objective, Objective::Balanced) && !policy_qualified
    {
        reasons.push("balanced_without_quality_policy".to_owned());
    }
    push_selection_reasons(
        &mut reasons,
        input,
        chosen,
        sessions,
        policy_qualified,
        has_benchmark,
    );
    let (fits, reasons) = sent_window_fit(input, chosen, &options, reasons);
    let graded = route_evidence(policy_qualified, has_benchmark, fits);
    enforce_min_confidence(input, graded.confidence, graded.evidence, &chosen.name)?;
    // Why every other eligible candidate lost. Without this the router is a black box that names a
    // winner; with it, a caller can see the comparison and disagree with it.
    let rejected = rejected_candidates(&eligible, &chosen.name, input.task, capability);
    Ok(RouteDecision {
        selected_model: chosen.name.clone(),
        task: input.task,
        objective: input.objective,
        profile,
        required_capabilities: required,
        options,
        think,
        strict_tool_validation,
        resident: chosen.resident,
        session_id: input.session_id.clone(),
        confidence: graded.confidence.to_owned(),
        evidence: graded.evidence.to_owned(),
        quality_evidence: graded.quality_evidence.to_owned(),
        task_evidence: graded.task_evidence.to_owned(),
        context_window_fit: graded.context_window_fit.to_owned(),
        hardware_fit: graded.hardware_fit.to_owned(),
        rejected,
        reasons,
    })
}

/// Why `chosen` won: an explicit model, session affinity, or its evidence tier.
fn push_selection_reasons(
    reasons: &mut Vec<String>,
    input: &RouteInput,
    chosen: &CatalogModel,
    sessions: &SessionAffinity,
    policy_qualified: bool,
    has_benchmark: bool,
) {
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
}

/// Grade the window we will actually send.
///
/// The task profile default (16k for chat, 2k for embeddings) is often larger than a small
/// model's advertised context; `profile()` already clamps `num_ctx` down, so the clamped value is
/// graded. Grading the unclamped default would report `insufficient_context` for a small embed
/// model that runs fine at its own window. Memory-budget fit lives in execution admission, not
/// this field.
fn sent_window_fit(
    input: &RouteInput,
    chosen: &CatalogModel,
    options: &Value,
    mut reasons: Vec<String>,
) -> (Option<bool>, Vec<String>) {
    let sent_ctx = options.get("num_ctx").and_then(Value::as_u64);
    if sent_ctx.is_some_and(|n| requested_context(input) > n) {
        reasons.push("context_clamped_to_advertised".to_owned());
    }
    let fits = chosen
        .advertised_context
        .map(|window| sent_ctx.is_some_and(|n| n <= window));
    (fits, reasons)
}

/// Refuse a route graded below the caller's requested minimum.
///
/// Deliberately verbose: a refusal the caller cannot act on is just an error. It names the grade,
/// the evidence behind it, the model that would have been chosen, and the two inputs that raise
/// the grade.
///
/// # Errors
///
/// Returns an error when `min_confidence` is set and the graded confidence ranks below it.
fn enforce_min_confidence(
    input: &RouteInput,
    confidence: &str,
    evidence: &str,
    would_select: &str,
) -> Result<()> {
    let Some(minimum) = input.min_confidence.as_deref() else {
        return Ok(());
    };
    // An unrecognised *minimum* must not silently disable the gate. Ranking it lowest would make
    // `min_confidence: "high"` — or any typo — accept every route, which is precisely the
    // fail-open behaviour this function exists to prevent, and the caller would never see it
    // because a passing gate is indistinguishable from no gate.
    let Some(required) = confidence_rank(minimum) else {
        bail!(
            "unknown min_confidence \"{minimum}\": the router grades routes \"low\" or \"medium\" \
             only — there is no \"high\". Refusing rather than ignoring the setting, because an \
             ignored floor looks exactly like a satisfied one."
        );
    };
    ensure!(
        confidence_rank(confidence).unwrap_or(0) >= required,
        "route refused: confidence is \"{confidence}\" (evidence: {evidence}), below the requested \
         minimum \"{minimum}\". Would have selected \"{would_select}\". This is a fail-closed \
         refusal, not a failure — the router cannot justify this pick. Lower `min_confidence`, or \
         raise the evidence level with a policy file (`freellama policy from-eval`) AND a \
         benchmark report (`freellama bench-all`) — note that naming an explicit `model` does NOT \
         raise the grade, because the grade measures the evidence, not who chose."
    );
    Ok(())
}

/// Ordering over confidence grades. `None` means the string is not a grade this router issues.
fn confidence_rank(grade: &str) -> Option<u8> {
    match grade {
        "low" => Some(1),
        "medium" => Some(2),
        _ => None,
    }
}

/// Why every eligible candidate other than the winner lost.
///
/// Naming only the winner makes the comparison unauditable: a caller cannot distinguish a
/// considered rejection from a model the router never looked at.
fn rejected_candidates(
    eligible: &[&CatalogModel],
    chosen: &str,
    task: TaskKind,
    capability: Capability,
) -> Vec<Value> {
    eligible
        .iter()
        .filter(|m| m.name != chosen)
        .map(|m| {
            json!({
                "model": m.name,
                "reason": if m.policy_rank.contains_key(&task) {
                    "policy_qualified_but_ranked_lower"
                } else if m.benchmark.contains_key(&capability) {
                    "benchmarked_but_not_policy_qualified"
                } else {
                    "capability_metadata_only"
                },
                "resident": m.resident,
            })
        })
        .collect()
}

/// The independent dimensions a route is graded on, reported separately.
///
/// `confidence: "medium"` on its own invites being read as a calibrated probability. It is not one:
/// it means "a policy vouched for this model on this task" AND "a functional benchmark exists" —
/// two different claims about two different kinds of evidence. The dimensions are reported
/// separately and `confidence` is derived from them.
#[derive(Debug, Clone)]
struct RouteEvidence {
    /// A policy file vouched for this model on this task.
    pub quality_evidence: &'static str,
    /// A functional benchmark measured this model on the ranking capability.
    pub task_evidence: &'static str,
    /// Sent `num_ctx` vs the model's advertised window. This is intentionally not named
    /// hardware fit: it proves neither live RAM/KV headroom nor successful runner admission.
    pub context_window_fit: &'static str,
    /// Deprecated compatibility alias for `context_window_fit`. Never interpret this as a live
    /// hardware-memory result; use `memory_kv_preflight` and post-run observation instead.
    pub hardware_fit: &'static str,
    /// Legacy single-word summary, derived from the three above. Kept so existing callers and the
    /// `min_confidence` gate keep working.
    pub confidence: &'static str,
    /// The strongest evidence class behind the pick, for one-line logging.
    pub evidence: &'static str,
}

fn route_evidence(
    policy_qualified: bool,
    has_benchmark: bool,
    fits: Option<bool>,
) -> RouteEvidence {
    let (confidence, evidence) = match (policy_qualified, has_benchmark) {
        (true, true) => ("medium", "configured_task_policy"),
        (true, false) => ("low", "configured_task_policy"),
        (false, true) => ("low", "functional_throughput_screen"),
        (false, false) => ("low", "capability_metadata_only"),
    };
    RouteEvidence {
        quality_evidence: if policy_qualified { "strong" } else { "none" },
        task_evidence: if has_benchmark { "strong" } else { "none" },
        context_window_fit: match fits {
            Some(true) => "fits_advertised_window",
            Some(false) => "insufficient_context",
            None => "unknown",
        },
        hardware_fit: match fits {
            Some(true) => "context_window_only",
            Some(false) => "insufficient_context",
            None => "unknown",
        },
        confidence,
        evidence,
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

/// Whether automatic selection (fallback ranking or session affinity) may prefer this model for
/// the task. A model observed to keep looping on this kind of task ranks last for it; an operator
/// policy naming the model for the task always qualifies it, and an explicit model always wins.
fn general_fallback_fit(model: &CatalogModel, task: TaskKind) -> bool {
    model.policy_rank.contains_key(&task)
        || !model
            .observed_loops
            .get(&task)
            .is_some_and(|record| record.keeps_looping())
}

fn compare_candidates(left: &CatalogModel, right: &CatalogModel, input: &RouteInput) -> Ordering {
    let capability = ranking_capability(input.task);
    let left_score = left.benchmark.get(&capability).copied();
    let right_score = right.benchmark.get(&capability).copied();
    let fit = general_fallback_fit(left, input.task).cmp(&general_fallback_fit(right, input.task));
    match input.objective {
        Objective::Fastest => fit
            .then_with(|| {
                left_score
                    .unwrap_or(0.0)
                    .total_cmp(&right_score.unwrap_or(0.0))
            })
            .then_with(|| right.size.cmp(&left.size))
            .then_with(|| right.name.cmp(&left.name)),
        Objective::Balanced => fit
            .then_with(|| left_score.is_some().cmp(&right_score.is_some()))
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

fn eligible_for_objective<'a>(
    input: &RouteInput,
    capability_eligible: Vec<&'a CatalogModel>,
) -> Result<Vec<&'a CatalogModel>> {
    if input.model.is_some() || matches!(input.objective, Objective::Fastest) {
        return Ok(capability_eligible);
    }
    let qualified = capability_eligible
        .iter()
        .copied()
        .filter(|model| model.policy_rank.contains_key(&input.task))
        .collect::<Vec<_>>();
    if qualified.is_empty() && matches!(input.objective, Objective::Balanced) {
        // Ordinary requests need no quality-policy setup. Retain capability/context filters
        // and report missing quality evidence; an explicit confidence floor still applies.
        return Ok(capability_eligible);
    }
    ensure!(
        !qualified.is_empty(),
        "no quality-qualified model exists for this task; configure a task policy (`freellama policy from-eval`), choose objective \"fastest\" (CLI: --objective fastest; MCP: objective:\"fastest\"), or name an explicit model. Run `freellama doctor` — unset OLLAMA_MAX_LOADED_MODELS / OLLAMA_KV_CACHE_TYPE is a machine-config gap, not a missing model"
    );
    Ok(qualified)
}

fn policy_preference(left: &CatalogModel, right: &CatalogModel, task: TaskKind) -> Ordering {
    let left_rank = left.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    let right_rank = right.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    right_rank.cmp(&left_rank)
}

/// Request options and validation mode chosen for one routed model.
struct TaskProfile {
    name: String,
    options: Value,
    think: Value,
    strict_tool_validation: bool,
    /// True when a policy `model_options` entry changed the generic task profile.
    policy_model_options: bool,
    /// True when the policy asked for `think = true` on a model without the Thinking capability.
    policy_think_dropped: bool,
}

fn profile(input: &RouteInput, model: &CatalogModel) -> TaskProfile {
    let policy = model.policy_options.get(&input.task).copied();
    let requested_context = requested_context(input);
    let num_ctx = requested_context
        .min(model.advertised_context.unwrap_or(requested_context))
        .min(u64::from(u32::MAX));
    let (name, num_predict, think, strict) = match input.task {
        TaskKind::Browser => ("browser_action", 64, Value::Bool(false), true),
        TaskKind::Tools => ("tools", 256, Value::Bool(false), true),
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
    let num_predict = policy
        .and_then(|entry| entry.num_predict)
        .unwrap_or(num_predict);
    // `think = true` only reaches a model that can think; Ollama rejects it otherwise.
    let policy_think = policy.and_then(|entry| entry.think);
    let policy_think_dropped =
        policy_think == Some(true) && !model.capabilities.contains(&Capability::Thinking);
    let think = match policy_think {
        Some(true) => thinking_or_off(model, true),
        Some(false) => Value::Bool(false),
        None => think,
    };
    let mut options = json!({"num_ctx": num_ctx});
    if num_predict > 0 {
        options["num_predict"] = json!(num_predict);
    }
    TaskProfile {
        name: name.to_owned(),
        options,
        think,
        strict_tool_validation: strict,
        policy_model_options: policy.is_some_and(|entry| entry != ModelRequestDefaults::default()),
        policy_think_dropped,
    }
}

fn requested_context(input: &RouteInput) -> u64 {
    input.context_tokens.unwrap_or(match input.task {
        TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair => 8_192,
        TaskKind::LongContext => 32_768,
        // Embeddings are not chat: embedding models commonly advertise 2048, so the chat default
        // would make every embed look like a context miss.
        TaskKind::Embedding => 2_048,
        _ => 16_384,
    })
}

fn thinking_or_off(model: &CatalogModel, effort: impl Into<Value>) -> Value {
    if model.capabilities.contains(&Capability::Thinking) {
        effort.into()
    } else {
        Value::Bool(false)
    }
}

#[cfg(test)]
mod task_kind_tests {
    use clap::ValueEnum;

    use super::TaskKind;

    #[test]
    fn as_str_matches_the_serde_wire_name_for_every_task() {
        for task in TaskKind::value_variants() {
            assert_eq!(
                serde_json::to_value(task).unwrap(),
                serde_json::json!(task.as_str())
            );
        }
    }
}
