//! Read-only, model-aware execution previews. No permits or model loads occur here.
use super::{
    PlatformState, TaskKind,
    execution::{ManagedDecision, model_memory_requirement, upstream_is_loopback},
    resources::{ResourceCapacityAssessment, ResourceDemand, ResourceSnapshot},
};
use serde::Serialize;
use serde_json::Value;

pub(super) struct CapacityPreview {
    pub(super) resources: ResourceSnapshot,
    pub(super) assessment: Option<ResourceCapacityAssessment>,
    pub(super) footprint: Value,
    pub(super) plan: AgentPlan,
    pub(super) slots_available: usize,
}

#[derive(Serialize)]
pub(super) struct AgentPlan {
    queue_readiness: &'static str,
    resource_readiness: &'static str,
    dispatch_readiness: &'static str,
    snapshot_only: bool,
    task_cost_units: u32,
    independent_tasks_admissible_now: usize,
    additional_independent_tasks_same_backend: usize,
    parallelism_rule: &'static str,
    memory_capacity_rule: &'static str,
    warm_runner_reuse_likely: bool,
    keep_alive_guidance: &'static str,
}

pub(super) async fn assess(
    decision: &ManagedDecision,
    state: &PlatformState,
    task_cost: u32,
) -> CapacityPreview {
    // Share the execution estimator, including exact digest/context residency and reservations.
    // Observe resources after metadata I/O; everything remains advisory and is rechecked at dispatch.
    let footprint =
        model_memory_requirement(state, &decision.execution, &decision.model, &decision.route)
            .await;
    let resources = state.resources.snapshot().await;
    let required = footprint["required_available_bytes"].as_u64().unwrap_or(0);
    let demand = if required == 0 && footprint["source"] == "matching_resident_context" {
        ResourceDemand::resident()
    } else {
        ResourceDemand::load(required)
    };
    let assessment = upstream_is_loopback(&decision.execution.upstream)
        .then(|| resources.assess_demand(demand, false));
    let task_cost = task_cost
        .min(u32::try_from(decision.execution.admission.total()).unwrap_or(u32::MAX))
        .max(1);
    let slots_available = decision.execution.admission.available();
    let queue_capacity = slots_available / usize::try_from(task_cost).unwrap_or(usize::MAX);
    let queue_readiness = if queue_capacity > 0 {
        "runnable_now"
    } else {
        "queue_likely"
    };
    let resource_readiness = assessment
        .as_ref()
        .map_or("not_applicable_remote", |value| value.status);
    let dispatch_readiness = match assessment.as_ref() {
        Some(value) if !value.admissible => value.status,
        Some(value) if value.status == "ready_telemetry_unknown" && queue_capacity > 0 => {
            "runnable_telemetry_unknown"
        }
        _ => queue_readiness,
    };
    let capacity = match assessment.as_ref() {
        Some(value) if !value.admissible => 0,
        Some(value) if required > 0 => {
            value
                .effective_available_bytes
                .map_or(queue_capacity, |available| {
                    queue_capacity.min(
                        usize::try_from(available.saturating_sub(value.reserve_bytes) / required)
                            .unwrap_or(usize::MAX),
                    )
                })
        }
        _ => queue_capacity,
    };
    let keep_alive_guidance = match decision.route.task {
        TaskKind::Embedding => {
            "use_keep_alive_0_for_one_off_embedding; keep_warm_only_for_a_related_batch"
        }
        _ if decision.route.resident => {
            "selected_runner_is_resident; preserve_prompt_prefix_and_keep_related_work_on_this_model"
        }
        _ => "selected_runner_is_cold; expect_a_load_transition_before_reusing_it",
    };
    CapacityPreview {
        resources,
        assessment,
        footprint,
        slots_available,
        plan: AgentPlan {
            queue_readiness,
            resource_readiness,
            dispatch_readiness,
            snapshot_only: true,
            task_cost_units: task_cost,
            independent_tasks_admissible_now: capacity,
            additional_independent_tasks_same_backend: capacity.saturating_sub(1),
            parallelism_rule: "the proposed task is included in admissible_now; launch only caller-declared independent siblings; execution admission remains authoritative",
            memory_capacity_rule: "conservative per-task memory estimate without shared-load credit; snapshots are not reservations or proof of concurrent runner execution",
            warm_runner_reuse_likely: decision.route.resident,
            keep_alive_guidance,
        },
    }
}
