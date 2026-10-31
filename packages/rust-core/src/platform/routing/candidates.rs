//! Candidate eligibility and ordering for route selection.
use super::{
    BTreeSet, Capability, CatalogModel, Objective, Ordering, Result, RouteInput, TaskKind, ensure,
};

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

pub(super) fn ranking_capability(task: TaskKind) -> Capability {
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
pub(super) fn general_fallback_fit(model: &CatalogModel, task: TaskKind) -> bool {
    model.policy_rank.contains_key(&task)
        || !model
            .observed_loops
            .get(&task)
            .is_some_and(|record| record.keeps_looping())
}

pub(super) fn compare_candidates(
    left: &CatalogModel,
    right: &CatalogModel,
    input: &RouteInput,
) -> Ordering {
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

pub(super) fn eligible_for_objective<'a>(
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

pub(super) fn policy_preference(
    left: &CatalogModel,
    right: &CatalogModel,
    task: TaskKind,
) -> Ordering {
    let left_rank = left.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    let right_rank = right.policy_rank.get(&task).copied().unwrap_or(usize::MAX);
    right_rank.cmp(&left_rank)
}
