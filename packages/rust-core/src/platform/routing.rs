//! Pure, side-effect-free routing: task/objective types, the model catalog view, session
//! affinity, and the evidence-graded `select_route` decision. No HTTP, no Ollama I/O — this is the
//! part the contract tests drive directly, kept separate so the decision logic can be read and
//! tested without the server plumbing around it.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
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
    /// Refuse the route rather than return one graded below this.
    ///
    /// This gate used to live only in the TypeScript MCP wrapper, so the CLI, the HTTP API and
    /// anyone embedding `freellama-core` as a library got a router with no fail-closed protection
    /// at all — while the documentation described it as a property of the platform. It belongs
    /// where the decision is made, not in one of three consumers.
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
        }
    }
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
    /// The dimensions `confidence` is derived from, reported separately so routing is inspectable
    /// rather than a single word a caller may mistake for a calibrated probability.
    pub quality_evidence: String,
    pub task_evidence: String,
    pub hardware_fit: String,
    /// Every other eligible candidate and why it lost.
    pub rejected: Vec<Value>,
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

    pub(super) fn create(&mut self) -> String {
        let id = Uuid::new_v4().to_string();
        self.sessions.insert(id.clone(), None);
        id
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    fn assigned(&self, id: &str) -> Option<&str> {
        self.sessions.get(id).and_then(Option::as_deref)
    }

    pub(super) fn bind(&mut self, id: &str, model: &str) {
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
    // `fits` is knowable here only as "we did not exclude it for context"; the memory budget lives
    // in the machine profile, so this dimension is honest about being weaker than the other two.
    // Unknown when the model advertises no context window — reported as "unknown", never as a pass.
    let fits = chosen
        .advertised_context
        .map(|window| requested_context(input) <= window);
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
        stream,
        strict_tool_validation,
        resident: chosen.resident,
        session_id: input.session_id.clone(),
        confidence: graded.confidence.to_owned(),
        evidence: graded.evidence.to_owned(),
        quality_evidence: graded.quality_evidence.to_owned(),
        task_evidence: graded.task_evidence.to_owned(),
        hardware_fit: graded.hardware_fit.to_owned(),
        rejected,
        reasons,
    })
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
         raise the evidence level with a policy file (`freellama policy-from-eval`) AND a \
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
/// two different claims about two different kinds of evidence, collapsed into one word. Collapsing
/// them is what makes routing uninspectable, so the dimensions are now first-class and `confidence`
/// is *derived* from them rather than being the only thing reported.
#[derive(Debug, Clone, Serialize)]
pub struct RouteEvidence {
    /// A policy file vouched for this model on this task.
    pub quality_evidence: &'static str,
    /// A functional benchmark measured this model on the ranking capability.
    pub task_evidence: &'static str,
    /// The model fits the machine's memory budget with room for its KV cache.
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
        hardware_fit: match fits {
            Some(true) => "strong",
            Some(false) => "insufficient_context",
            None => "unknown",
        },
        confidence,
        evidence,
    }
}

pub(super) fn requirements(input: &RouteInput) -> BTreeSet<Capability> {
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

pub(super) fn requested_context(input: &RouteInput) -> u64 {
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
