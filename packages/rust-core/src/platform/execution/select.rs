//! Managed route selection: candidates, observed-placement checks and routing profiles.

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use super::super::error::ApiError;
use super::super::{
    Backend, CatalogModel, ExecutionPreference, PlacementEvidence, PlacementSignals, PlatformState,
    RouteDecision, RouteInput, SessionAffinity, TaskInput, TaskKind, desired_placement,
    select_route,
};
use super::{
    ExecutionTarget, apply_execution_options, apply_request_options, execution_target,
    memory_kv_preflight, physical_placement_observation, resident_entry,
};

pub(crate) struct ManagedDecision {
    pub(crate) route: RouteDecision,
    pub(crate) model: CatalogModel,
    pub(crate) execution: ExecutionTarget,
    pub(crate) preference: ExecutionPreference,
    preference_satisfied: bool,
    pub(crate) reason: &'static str,
    placement_evidence: PlacementEvidence,
}

impl ManagedDecision {
    pub(crate) async fn execution_receipt(&self, task_cost: u32, state: &PlatformState) -> Value {
        let capacity = super::super::readiness::assess(self, state, task_cost).await;
        json!({
            // `placement` is retained for compatibility. `backend` names the configured Ollama
            // process; neither field is physical proof. The task response replaces the pending
            // observation below with Ollama's post-run `/api/ps` evidence.
            "placement": self.execution.placement,
            "backend": if self.execution.placement == Backend::Cpu { "cpu" } else { "primary" },
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
                "slots_total": self.execution.admission.total(),
                "slots_available": capacity.slots_available,
            },
            "resource_snapshot": capacity.resources,
            "resource_assessment": capacity.assessment,
            "memory_reservation": capacity.footprint,
            "agent_plan": capacity.plan,
            "memory_kv_preflight": memory_kv_preflight(&self.model, &self.route, self.execution.placement, &self.execution.upstream),
        })
    }
}

fn route_candidate_for(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
    placement: Backend,
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
        selected.resident_size,
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

pub(crate) async fn select_managed_route(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
    task_cost_units: u32,
    profile_input: Option<&TaskInput>,
) -> Result<ManagedDecision, ApiError> {
    let has_session_affinity = input
        .session_id
        .as_deref()
        .and_then(|id| sessions.assigned(id))
        .is_some();
    let route_is_pinned = input.model.is_some() || has_session_affinity;
    let gpu_candidate = route_candidate_for(state, input, models, sessions, Backend::Gpu);
    let cpu_candidate = route_candidate_for(state, input, models, sessions, Backend::Cpu);
    let (gpu_work_unit_ns, cpu_work_unit_ns) = if route_is_pinned {
        (None, None)
    } else {
        matching_route_profile_scores(
            state,
            input,
            models,
            task_cost_units,
            profile_input,
            (gpu_candidate.as_ref(), cpu_candidate.as_ref()),
        )
        .await
    };
    let desired = desired_placement(PlacementSignals {
        route_is_pinned,
        objective: input.objective,
        execution_preference: input.execution_preference,
        gpu_work_unit_ns,
        cpu_work_unit_ns,
        gpu_slots_available: state.gpu_admission.available(),
        cpu_slots_available: state.cpu_admission.available(),
        gpu_task_cost: usize::try_from(
            task_cost_units.min(u32::try_from(state.gpu_admission.total()).unwrap_or(u32::MAX)),
        )
        .unwrap_or(usize::MAX)
        .max(1),
        cpu_task_cost: usize::try_from(
            task_cost_units.min(u32::try_from(state.cpu_admission.total()).unwrap_or(u32::MAX)),
        )
        .unwrap_or(usize::MAX)
        .max(1),
        cpu_configured: state.cpu_upstream.is_some(),
    });

    let preferred = desired.and_then(|(placement, reason)| {
        if route_is_pinned {
            return None;
        }
        (if placement == Backend::Cpu {
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
        ExecutionPreference::PreferCpu => execution.placement == Backend::Cpu,
        ExecutionPreference::PreferGpu => execution.placement == Backend::Gpu,
    };
    let model = models
        .iter()
        .find(|model| model.name == route.selected_model)
        .expect("selected route comes from the supplied catalog")
        .clone();
    Ok(ManagedDecision {
        route,
        model,
        execution,
        preference: input.execution_preference,
        preference_satisfied,
        reason,
        placement_evidence: input.min_placement_evidence,
    })
}

async fn matching_route_profile_scores(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    task_cost_units: u32,
    profile_input: Option<&TaskInput>,
    candidates: (Option<&RouteDecision>, Option<&RouteDecision>),
) -> (Option<u128>, Option<u128>) {
    if input.execution_preference != ExecutionPreference::Auto
        || !matches!(
            input.objective,
            super::super::Objective::Fastest | super::super::Objective::Balanced
        )
    {
        return (None, None);
    }
    let (Some(gpu), Some(cpu), Some(payload)) = (candidates.0, candidates.1, profile_input) else {
        return (None, None);
    };
    let (gpu_profile, cpu_profile) = tokio::join!(
        routing_profile(state, input, gpu, models, task_cost_units, payload),
        routing_profile(state, input, cpu, models, task_cost_units, payload)
    );
    let feedback = state.feedback.read().await;
    (
        gpu_profile.as_ref().and_then(|profile| {
            feedback
                .gpu
                .get(&input.task)
                .and_then(|stats| stats.average_for_profile(profile))
        }),
        cpu_profile.as_ref().and_then(|profile| {
            feedback
                .cpu
                .get(&input.task)
                .and_then(|stats| stats.average_for_profile(profile))
        }),
    )
}

#[derive(Clone, Copy)]
pub(crate) struct ProfileEvidence<'a> {
    pub(crate) task: TaskKind,
    pub(crate) model: &'a str,
    pub(crate) catalog_digest: Option<&'a str>,
    pub(crate) upstream: &'a str,
    pub(crate) body: &'a Value,
    pub(crate) placement: &'a Value,
    pub(crate) dispatch_admission: &'a Value,
    pub(crate) completion_admission: &'a Value,
    pub(crate) dispatch_settings: Option<&'a Value>,
    pub(crate) completion_settings: Option<&'a Value>,
    pub(crate) warm: bool,
}

pub(crate) fn unchanged_admission_interval(start: &Value, end: &Value) -> bool {
    [
        "admitted",
        "released",
        "policy_epoch",
        "slots_total",
        "slots_ceiling",
        "active_units",
        "in_flight",
    ]
    .into_iter()
    .all(|field| {
        start[field]
            .as_u64()
            .is_some_and(|value| end[field].as_u64() == Some(value))
    })
}

pub(crate) fn profile_from_observations(
    evidence: ProfileEvidence<'_>,
) -> Result<super::super::runtime::ExecutionProfile, &'static str> {
    if evidence.placement["status"] != "verified" {
        return Err("physical_placement_unverified");
    }
    let processor = evidence.placement["processor"]
        .as_str()
        .ok_or("processor_identity_unknown")?;
    let digest = evidence.placement["digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .ok_or("model_revision_unknown")?;
    if evidence.catalog_digest != Some(digest) {
        return Err("model_revision_mismatch_or_unknown");
    }
    let context = evidence.placement["context_length"]
        .as_u64()
        .filter(|context| *context > 0)
        .ok_or("observed_context_unknown")?;
    let options = evidence.body["options"]
        .as_object()
        .ok_or("effective_options_unknown")?;
    if options.get("num_ctx").and_then(Value::as_u64) != Some(context) {
        return Err("effective_context_mismatch_or_unknown");
    }
    if !evidence.warm {
        return Err("runner_transition_not_comparable");
    }
    if evidence.dispatch_admission["in_flight"] != 1
        || evidence.completion_admission["in_flight"] != 1
        || !unchanged_admission_interval(evidence.dispatch_admission, evidence.completion_admission)
    {
        return Err("admission_occupancy_or_policy_not_comparable");
    }
    let settings = evidence
        .dispatch_settings
        .ok_or("backend_process_settings_unknown")?;
    if evidence.completion_settings != Some(settings) {
        return Err("backend_process_settings_changed_or_unknown");
    }
    let controls: serde_json::Map<String, Value> = [
        "options",
        "think",
        "format",
        "tools",
        "logprobs",
        "top_logprobs",
        "dimensions",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), evidence.body[key].clone()))
    .collect();
    let metric_kind = super::super::runtime::ThroughputMetricKind::for_task(evidence.task);
    let mut material = json!({
        "metric_kind": metric_kind,
        "task": evidence.task, "model": evidence.model, "digest": digest,
        "upstream": evidence.upstream, "processor": processor, "context_tokens": context,
        "controls": controls, "backend_settings": settings,
        "admission_class": {"in_flight":1,"limit":evidence.dispatch_admission["slots_total"],
            "ceiling":evidence.dispatch_admission["slots_ceiling"],"charged_units":evidence.dispatch_admission["active_units"]},
    });
    material.sort_all_objects();
    let bytes = serde_json::to_vec(&material).expect("profile material serializes");
    Ok(super::super::runtime::ExecutionProfile {
        id: Sha256::digest(bytes)
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
        task: evidence.task,
        model: evidence.model.to_owned(),
        digest: digest.to_owned(),
        context_tokens: context,
        processor: processor.to_owned(),
        metric_kind,
    })
}

pub(crate) async fn comparable_backend_settings(
    _state: &PlatformState,
    execution: &ExecutionTarget,
) -> Option<Value> {
    let endpoint = execution.upstream.clone();
    tokio::task::spawn_blocking(move || {
        super::super::ollama_env::probe_comparable_process(&endpoint)
    })
    .await
    .ok()
    .flatten()
}

async fn routing_profile(
    state: &PlatformState,
    input: &RouteInput,
    route: &RouteDecision,
    models: &[CatalogModel],
    requested_cost: u32,
    payload: &TaskInput,
) -> Option<super::super::runtime::ExecutionProfile> {
    input.context_tokens?;
    if matches!(payload.operation, super::super::ManagedOperation::Warm) {
        return None;
    }
    let execution = execution_target(state, &route.selected_model);
    let before = execution.admission.receipt();
    if before["in_flight"] != 0 || before["active_units"] != 0 {
        return None;
    }
    let settings = comparable_backend_settings(state, &execution).await?;
    let loaded = resident_entry(&state.client, &execution.upstream, &route.selected_model).await?;
    let after = execution.admission.receipt();
    if !unchanged_admission_interval(&before, &after) {
        return None;
    }
    let mut effective = route.clone();
    apply_request_options(input.task, &mut effective, &payload.request_options).ok()?;
    let mut body = if matches!(input.task, TaskKind::Embedding) {
        json!({"options":effective.options})
    } else {
        json!({"options":effective.options,"think":effective.think,"tools":payload.tools,
            "format":payload.request_options.format,"logprobs":payload.request_options.logprobs,
            "top_logprobs":payload.request_options.top_logprobs})
    };
    apply_execution_options(&mut body, &execution);
    let model = models
        .iter()
        .find(|model| model.name == route.selected_model)?;
    let mut placement = physical_placement_observation(
        execution.placement,
        loaded["size"].as_u64(),
        loaded["size_vram"].as_u64(),
    );
    placement["digest"] = loaded["digest"].clone();
    placement["context_length"] = loaded["context_length"].clone();
    let mut prospective = before;
    prospective["in_flight"] = json!(1);
    prospective["active_units"] = json!(
        u64::from(requested_cost)
            .min(prospective["slots_total"].as_u64()?)
            .max(1)
    );
    profile_from_observations(ProfileEvidence {
        task: input.task,
        model: &route.selected_model,
        catalog_digest: model.digest.as_deref(),
        upstream: &execution.upstream,
        body: &body,
        placement: &placement,
        dispatch_admission: &prospective,
        completion_admission: &prospective,
        dispatch_settings: Some(&settings),
        completion_settings: Some(&settings),
        warm: true,
    })
    .ok()
}
