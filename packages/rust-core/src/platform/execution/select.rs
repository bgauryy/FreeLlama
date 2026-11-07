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
    completion_planning: Value,
}

impl ManagedDecision {
    pub(crate) async fn execution_receipt(&self, task_cost: u32, state: &PlatformState) -> Value {
        let capacity = super::super::readiness::assess(self, state, task_cost).await;
        json!({
            // `backend` names the configured Ollama process and `requested_processor` the
            // requested device; neither is physical proof. The task response replaces the pending
            // observation below with Ollama's post-run `/api/ps` evidence.
            "backend": if self.execution.placement == Backend::Cpu { "cpu" } else { "primary" },
            "requested_processor": self.execution.placement,
            "upstream": self.execution.upstream,
            "preference": self.preference,
            "preference_satisfied": self.preference_satisfied,
            "reason": self.reason,
            "completion_planning": self.completion_planning,
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
            // A load that does not fit for lack of memory names who holds it, so the caller can
            // ask its user what to close.
            "memory_holders": capacity.assessment.as_ref().is_some_and(|assessment| {
                assessment.denial_reason
                    == Some(super::super::resources::CapacityDenialReason::InsufficientCapacity)
                    || assessment.held_by.iter().any(|reason| reason.is_memory())
            }).then(super::super::resources::memory_holders),
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

/// Check payload fit before choosing a backend/model, without changing caller pins or prompts.
fn payload_compatible_models(
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
    payload: Option<&TaskInput>,
    pinned: bool,
) -> Result<(Vec<CatalogModel>, Vec<Value>), ApiError> {
    if pinned {
        return Ok((models.to_vec(), Vec::new()));
    }
    let eligible =
        super::super::routing::eligible_models(input, models).map_err(ApiError::bad_request)?;
    let Some(payload) = payload else {
        return Ok((eligible.into_iter().cloned().collect(), Vec::new()));
    };
    let mut compatible = Vec::new();
    let mut rejected = Vec::new();
    let mut first_error = None;
    let mut candidate_input = input.clone();
    // Confidence is enforced by the final router; inspect only the effective payload fit here.
    candidate_input.min_confidence = None;
    for model in eligible {
        let mut route = select_route(&candidate_input, std::slice::from_ref(model), sessions)
            .map_err(ApiError::bad_request)?;
        apply_request_options(input.task, &mut route, &payload.request_options)?;
        match super::request::validate_model_controls(model, &payload.request_options)
            .and_then(|()| super::super::context::size_context(payload, &mut route, model))
        {
            Ok(_) => compatible.push(model.clone()),
            Err(error) => {
                rejected.push(
                    json!({"model":model.name,"reason":"execution_payload_context_does_not_fit"}),
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    if compatible.is_empty()
        && let Some(error) = first_error
    {
        return Err(error);
    }
    Ok((compatible, rejected))
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

fn placement_hint(
    state: &PlatformState,
    input: &RouteInput,
    task_cost_units: u32,
    route_is_pinned: bool,
    candidates: (Option<&RouteDecision>, Option<&RouteDecision>),
    scores: (Option<u128>, Option<u128>),
) -> Option<(Backend, &'static str)> {
    let cost = |pool: &super::super::admission::AdmissionPool| {
        usize::try_from(task_cost_units)
            .unwrap_or(usize::MAX)
            .min(pool.total())
            .max(1)
    };
    desired_placement(PlacementSignals {
        route_is_pinned,
        objective: input.objective,
        execution_preference: input.execution_preference,
        gpu_work_unit_ns: scores.0,
        cpu_work_unit_ns: scores.1,
        gpu_slots_available: state.gpu_admission.available(),
        cpu_slots_available: state.cpu_admission.available(),
        gpu_task_cost: cost(&state.gpu_admission),
        cpu_task_cost: cost(&state.cpu_admission),
        cpu_configured: state.cpu_upstream.is_some(),
        gpu_resident: candidates.0.is_some_and(|route| route.resident),
        cpu_resident: candidates.1.is_some_and(|route| route.resident),
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "keep eligibility, affinity and receipt decisions together in one auditable selection path"
)]
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
    let (compatible, payload_rejections) =
        payload_compatible_models(input, models, sessions, profile_input, route_is_pinned)?;
    let models = compatible.as_slice();
    let gpu_candidate = route_candidate_for(state, input, models, sessions, Backend::Gpu);
    let cpu_candidate = route_candidate_for(state, input, models, sessions, Backend::Cpu);
    let (gpu_work_unit_ns, cpu_work_unit_ns, candidate_plans) = if route_is_pinned {
        (None, None, Vec::new())
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
    let completion_preference = super::completion::preferred(&candidate_plans);
    let complete_predictions = candidate_plans.len() == 2
        && candidate_plans
            .iter()
            .all(|plan| plan.estimated_completion_ns.is_some());
    // Once complete phase estimates exist, a decode-only score must not overturn their result.
    let (gpu_work_unit_ns, cpu_work_unit_ns) = if complete_predictions {
        (None, None)
    } else {
        (gpu_work_unit_ns, cpu_work_unit_ns)
    };
    let desired = completion_preference
        .map(|backend| (backend, "estimated_completion_faster"))
        .or_else(|| {
            placement_hint(
                state,
                input,
                task_cost_units,
                route_is_pinned,
                (gpu_candidate.as_ref(), cpu_candidate.as_ref()),
                (gpu_work_unit_ns, cpu_work_unit_ns),
            )
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
        completion_planning: super::completion::receipt(
            &candidate_plans,
            completion_preference.is_some(),
            &payload_rejections,
        ),
    })
}

async fn matching_route_profile_scores(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    task_cost_units: u32,
    profile_input: Option<&TaskInput>,
    candidates: (Option<&RouteDecision>, Option<&RouteDecision>),
) -> (
    Option<u128>,
    Option<u128>,
    Vec<super::completion::CandidatePlan>,
) {
    if input.execution_preference != ExecutionPreference::Auto
        || !matches!(
            input.objective,
            super::super::Objective::Fastest | super::super::Objective::Balanced
        )
    {
        return (None, None, Vec::new());
    }
    let (Some(gpu), Some(cpu), Some(payload)) = (candidates.0, candidates.1, profile_input) else {
        return (None, None, Vec::new());
    };
    let (gpu_profile, cpu_profile) = tokio::join!(
        routing_profile(state, input, gpu, models, task_cost_units, payload),
        routing_profile(state, input, cpu, models, task_cost_units, payload)
    );
    let feedback = state.feedback.read().await;
    let mut plans = Vec::with_capacity(2);
    let mut scores = [None, None];
    for (index, backend, route, evidence, history) in [
        (0, Backend::Gpu, gpu, gpu_profile, &feedback.gpu),
        (1, Backend::Cpu, cpu, cpu_profile, &feedback.cpu),
    ] {
        let mut plan = super::completion::CandidatePlan::unknown(
            &route.selected_model,
            backend,
            "not_compatible_resident_idle_or_observations_unknown",
        );
        if let Some((profile, input_tokens, output_tokens)) = evidence {
            plan.profile_id = Some(profile.id.clone());
            plan.status = "insufficient_phase_history";
            if let Some(stats) = history.get(&input.task) {
                scores[index] = stats.average_for_profile(&profile);
                if let (Some(phases), Some(tokens)) =
                    (stats.phases_for_profile(&profile), input_tokens)
                {
                    plan.estimate(&profile.id, &phases, tokens, output_tokens);
                }
            }
        }
        plans.push(plan);
    }
    (scores[0], scores[1], plans)
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

/// Ollama's process settings change only when it restarts, so one probe (`pgrep` + `ps`) per
/// endpoint serves every task for a few seconds instead of spawning two processes per task.
const SETTINGS_TTL: std::time::Duration = std::time::Duration::from_secs(5);

type SettingsCache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Option<Value>)>>;

fn settings_cache() -> &'static SettingsCache {
    static CACHE: std::sync::OnceLock<SettingsCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(crate) async fn comparable_backend_settings(
    _state: &PlatformState,
    execution: &ExecutionTarget,
) -> Option<Value> {
    let endpoint = execution.upstream.clone();
    if let Some((at, settings)) = settings_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&endpoint)
        && at.elapsed() < SETTINGS_TTL
    {
        return settings.clone();
    }
    let key = endpoint.clone();
    let settings = tokio::task::spawn_blocking(move || {
        super::super::ollama_env::probe_comparable_process(&endpoint)
    })
    .await
    .ok()
    .flatten();
    settings_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (std::time::Instant::now(), settings.clone()));
    settings
}

async fn routing_profile(
    state: &PlatformState,
    input: &RouteInput,
    route: &RouteDecision,
    models: &[CatalogModel],
    requested_cost: u32,
    payload: &TaskInput,
) -> Option<(super::super::runtime::ExecutionProfile, Option<u64>, u64)> {
    if matches!(payload.operation, super::super::ManagedOperation::Warm) {
        return None;
    }
    let execution = execution_target(state, &route.selected_model);
    let before = execution.admission.receipt();
    if before["in_flight"] != 0 || before["active_units"] != 0 || before["queue_depth"] != 0 {
        return None;
    }
    let settings = comparable_backend_settings(state, &execution).await?;
    let expected_digest = models
        .iter()
        .find(|model| model.name == route.selected_model)
        .and_then(|model| model.digest.as_deref());
    let loaded = resident_entry(
        &state.client,
        &execution.upstream,
        &route.selected_model,
        expected_digest,
    )
    .await?;
    let after = execution.admission.receipt();
    if !unchanged_admission_interval(&before, &after) {
        return None;
    }
    let model = models
        .iter()
        .find(|model| model.name == route.selected_model)?;
    let mut managed = ManagedDecision {
        route: route.clone(),
        model: model.clone(),
        execution: execution.clone(),
        preference: input.execution_preference,
        preference_satisfied: true,
        reason: "planning",
        placement_evidence: input.min_placement_evidence,
        completion_planning: Value::Null,
    };
    prepare_managed_context(state, payload, &mut managed)
        .await
        .ok()?;
    if crate::net::is_loopback_url(&execution.upstream)
        && !state
            .resources
            .snapshot()
            .await
            .assess_demand(
                super::super::resources::ResourceDemand::resident()
                    .on_cpu(execution.placement == Backend::Cpu),
                false,
            )
            .admissible
    {
        return None;
    }
    let body = request_body_for_loaded_runner(input, payload, &managed, &loaded)?;
    let input_tokens = super::super::context::estimated_work_tokens(payload, model);
    let output_tokens = if matches!(input.task, TaskKind::Embedding) {
        0
    } else {
        managed.route.options["num_predict"]
            .as_u64()
            .unwrap_or(managed.route.output_reserve_tokens)
    };
    // The preparation may inspect Ollama. Refuse a prediction if admission changed meanwhile.
    let final_admission = execution.admission.receipt();
    if final_admission["queue_depth"] != 0
        || !unchanged_admission_interval(&before, &final_admission)
    {
        return None;
    }
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
    let profile = profile_from_observations(ProfileEvidence {
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
    .ok()?;
    Some((profile, input_tokens, output_tokens))
}

/// The request controls a managed task would send, when the resident runner `loaded` already
/// satisfies them (digest, context, runtime options and, when required, GPU placement). `None`
/// means the task would reload the model, so its warm timing says nothing about this run.
fn request_body_for_loaded_runner(
    input: &RouteInput,
    payload: &TaskInput,
    managed: &ManagedDecision,
    loaded: &Value,
) -> Option<Value> {
    let mut body = super::request::profile_controls(payload, &managed.route);
    apply_execution_options(&mut body, &managed.execution);
    let mut expectation = super::runner::RunnerExpectation::new(&managed.model, &managed.route)
        .with_runtime_options(&body["options"]);
    if matches!(input.min_placement_evidence, PlacementEvidence::Observed) {
        expectation = expectation.requiring_gpu_placement(managed.execution.placement);
    }
    expectation.matches(loaded).then_some(body)
}

/// Prepare the same effective context for prediction and execution. Ollama owns automatic
/// context selection when its known default safely covers the request.
pub(super) async fn prepare_managed_context(
    state: &PlatformState,
    input: &TaskInput,
    managed: &mut ManagedDecision,
) -> Result<(Value, Option<Value>), ApiError> {
    apply_request_options(input.route.task, &mut managed.route, &input.request_options)?;
    super::request::validate_model_controls(&managed.model, &input.request_options)?;
    let mut sizing =
        super::super::context::size_context(input, &mut managed.route, &managed.model)?;
    if managed.route.resident && sizing["mode"] == "prompt_estimate" {
        let loaded = resident_entry(
            &state.client,
            &managed.execution.upstream,
            &managed.route.selected_model,
            managed.model.digest.as_deref(),
        )
        .await;
        if let Some(reused) = super::super::context::reuse_resident_context(
            &mut managed.route,
            &managed.model,
            loaded.as_ref(),
        ) {
            sizing["reused_resident_context"] = json!(reused);
        }
    }
    let automatic = context_left_to_ollama(state, managed, &sizing).await;
    Ok((sizing, automatic))
}

/// Decide whether to leave `num_ctx` to Ollama for this request.
///
/// Only for prompt-sized contexts (an explicit `context_tokens` is always sent), and only when
/// Ollama's default for this model is known, covers the sized context, and does not exceed
/// `auto_context_max` (a 256k tier default would allocate a huge KV cache). A resident runner
/// whose context was reused keeps being addressed explicitly. On success the decision's
/// `num_ctx` is set to the default so memory estimates match what Ollama will allocate.
async fn context_left_to_ollama(
    state: &PlatformState,
    managed: &mut ManagedDecision,
    context_sizing: &Value,
) -> Option<Value> {
    let tunables = state.tunables();
    if tunables.context_mode != super::super::ContextMode::OllamaDefault
        || context_sizing["mode"] != "prompt_estimate"
        || context_sizing.get("reused_resident_context").is_some()
    {
        return None;
    }
    let sized = managed.route.options["num_ctx"].as_u64()?;
    let (default, source) = super::super::monitor::ollama_default_context(
        state,
        Some(&managed.model),
        managed.execution.placement,
    )
    .await?;
    let window = managed.model.advertised_context.unwrap_or(u64::MAX);
    if default < sized || default > tunables.auto_context_max || default > window {
        return None;
    }
    // A runner already loaded with some other explicit context would be reloaded by an
    // automatic request; keep addressing it explicitly (reuse handles the larger-window case).
    if managed.route.resident {
        let loaded = resident_entry(
            &state.client,
            &managed.execution.upstream,
            &managed.route.selected_model,
            managed.model.digest.as_deref(),
        )
        .await;
        if loaded
            .as_ref()
            .and_then(|entry| entry["context_length"].as_u64())
            .is_some_and(|loaded| loaded != default)
        {
            return None;
        }
    }
    managed.route.options["num_ctx"] = json!(default);
    managed
        .route
        .reasons
        .push("context_left_to_ollama_default".to_owned());
    Some(json!({"tokens": default, "source": source}))
}

#[cfg(test)]
mod planning_tests {
    use super::*;

    fn model(name: &str, context: u64, qualified: bool) -> CatalogModel {
        serde_json::from_value(json!({
            "name":name,"digest":format!("digest-{name}"),"size":1024,
            "capabilities":["completion"],"advertised_context":context,
            "resident":false,"benchmark":{},
            "policy_rank":if qualified { json!({"completion":0}) } else { json!({}) },
        }))
        .unwrap()
    }

    #[test]
    fn payload_fit_is_checked_before_model_ranking() {
        let input: TaskInput = serde_json::from_value(json!({
            "task":"completion","objective":"fastest","prompt":"x".repeat(9000),
            "request_options":{"options":{"num_predict":100}}
        }))
        .unwrap();
        let models = vec![model("small", 2048, false), model("large", 8192, false)];
        let (compatible, rejected) = payload_compatible_models(
            &input.route,
            &models,
            &SessionAffinity::default(),
            Some(&input),
            false,
        )
        .unwrap();
        assert_eq!(compatible.len(), 1);
        assert_eq!(compatible[0].name, "large");
        assert_eq!(rejected[0]["model"], "small");
    }

    #[test]
    fn pinned_model_is_not_silently_replaced_when_payload_does_not_fit() {
        let input: TaskInput = serde_json::from_value(json!({
            "model":"small","prompt":"x".repeat(9000)
        }))
        .unwrap();
        let models = vec![model("small", 2048, false), model("large", 8192, false)];
        let (compatible, rejected) = payload_compatible_models(
            &input.route,
            &models,
            &SessionAffinity::default(),
            Some(&input),
            true,
        )
        .unwrap();
        let route = select_route(&input.route, &compatible, &SessionAffinity::default()).unwrap();
        assert_eq!(route.selected_model, "small");
        assert_eq!(rejected, Vec::<Value>::new());
    }

    #[test]
    fn backend_partition_cannot_relax_global_quality_eligibility() {
        let input = RouteInput::default();
        let models = vec![
            model("qualified", 8192, true),
            model("unqualified", 8192, false),
        ];
        let (compatible, _) =
            payload_compatible_models(&input, &models, &SessionAffinity::default(), None, false)
                .unwrap();
        assert_eq!(compatible.len(), 1);
        assert_eq!(compatible[0].name, "qualified");
    }

    #[test]
    fn payload_rejection_does_not_fall_back_outside_quality_gate() {
        let input: TaskInput = serde_json::from_value(json!({
            "prompt":"x".repeat(9000),"request_options":{"options":{"num_predict":100}}
        }))
        .unwrap();
        let models = vec![
            model("qualified-small", 2048, true),
            model("unqualified-large", 8192, false),
        ];
        assert!(
            payload_compatible_models(
                &input.route,
                &models,
                &SessionAffinity::default(),
                Some(&input),
                false,
            )
            .is_err()
        );
    }
}
