//! Managed execution: route selection, capacity reservation, forwarding, and evidence receipts.
use super::admission::{AdmissionFailure, AdmissionPermit, AdmissionPool};
use super::error::{ApiError, resource_error};
use super::{
    CatalogModel, ExecutionPreference, FEEDBACK_SCHEMA_VERSION, OllamaRequestOptions,
    PlacementEvidence, PlacementSignals, PlatformState, RouteDecision, RouteInput, SessionAffinity,
    TaskInput, TaskKind, TaskPriority, context, desired_placement, discover_models, get_json,
    host_has_unified_memory, host_total_memory_bytes, persist_feedback, require_active_session,
    residency, resources, select_route,
};
use crate::{model_bench::Capability, proxy};
use anyhow::{Context, Result};
use axum::{Json, extract::State, http::StatusCode};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// How long a request may wait for host memory while holding its admission slot.
const SLOT_HELD_RESOURCE_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct ExecutionTarget {
    pub(super) placement: &'static str,
    pub(super) upstream: String,
    pub(super) transition: Arc<RwLock<()>>,
    pub(super) admission: AdmissionPool,
}

enum TransitionPermit {
    Shared(OwnedRwLockReadGuard<()>),
    Exclusive(OwnedRwLockWriteGuard<()>),
}

impl TransitionPermit {
    fn admission_mode(&self) -> &'static str {
        match self {
            Self::Shared(_guard) => "resident_shared",
            Self::Exclusive(_guard) => "nonresident_transition_exclusive",
        }
    }
}

pub(super) fn execution_target(state: &PlatformState, model: &str) -> ExecutionTarget {
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

pub(super) struct ManagedDecision {
    pub(super) route: RouteDecision,
    pub(super) model: CatalogModel,
    pub(super) execution: ExecutionTarget,
    pub(super) preference: ExecutionPreference,
    pub(super) preference_satisfied: bool,
    pub(super) reason: &'static str,
    pub(super) placement_evidence: PlacementEvidence,
}

impl ManagedDecision {
    pub(super) async fn execution_receipt(&self, task_cost: u32, state: &PlatformState) -> Value {
        let capacity = super::readiness::assess(self, state, task_cost).await;
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

pub(super) async fn select_managed_route(
    state: &PlatformState,
    input: &RouteInput,
    models: &[CatalogModel],
    sessions: &SessionAffinity,
    task_cost_units: u32,
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

/// Thread count for CPU-placed runners, or `None` to let Ollama choose.
///
/// Ollama compares `num_thread` when deciding whether a loaded runner can serve a request, so a
/// value that differs from what other clients send forces a reload. It is therefore set only on
/// the dedicated CPU backend, where `FreeLlama` is the only client. The GPU backend is left alone:
/// injecting `logical/2` there made every raw request without `num_thread` reload the runner
/// managed traffic had just loaded, and on Apple Silicon (no SMT) it halved CPU throughput.
fn cpu_num_thread() -> Option<u64> {
    if let Some(value) = std::env::var("FREELLAMA_CPU_NUM_THREAD")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        return Some(value);
    }
    if cfg!(target_os = "macos") {
        // Ollama already sizes to performance cores; efficiency cores slow llama.cpp's barriers.
        return None;
    }
    // x86 SMT: half the logical CPUs approximates physical cores and leaves the siblings free.
    std::thread::available_parallelism()
        .ok()
        .map(|cores| (cores.get() as u64 / 2).max(1))
}

pub(super) fn apply_execution_options(body: &mut Value, target: &ExecutionTarget) {
    if target.placement == "cpu" {
        if let (Some(options), Some(threads)) = (
            body.get_mut("options").and_then(Value::as_object_mut),
            cpu_num_thread(),
        ) {
            options
                .entry("num_thread")
                .or_insert_with(|| json!(threads));
        }
        // The process-level CPU library override is ignored by some Metal builds. Ollama's
        // request contract treats num_gpu as a runner load option; pinning zero here makes the
        // explicit CPU assignment real while the second process prevents GPU-runner churn on the
        // primary backend.
        body["options"]["num_gpu"] = json!(0);
    }
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
                "truncate": false,
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
        "truncate": false,
        "shift": false,
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
pub(super) async fn admit(
    state: &PlatformState,
    execution: &ExecutionTarget,
    task: TaskKind,
    priority: TaskPriority,
    batch_items: usize,
    deadline: tokio::time::Instant,
) -> Result<(AdmissionPermit, u32, u128), ApiError> {
    let budget = u32::try_from(execution.admission.total())
        .unwrap_or(u32::MAX)
        .max(1);
    let cost = task_cost_for(task, batch_items).min(budget).max(1);
    let wait = state.tunables().max_queue_wait();
    match execution
        .admission
        .acquire(cost as usize, priority, deadline)
        .await
    {
        Ok((permit, queued)) => Ok((permit, cost, queued)),
        Err(failure) => {
            let retry_after = super::runtime::retry_after_seconds(
                execution.admission.queue_depth(),
                execution.admission.total(),
                state.average_task_ms(execution.placement),
            );
            let (status, code) = match failure {
                // A full queue is FreeLlama's own configured cap, not an upstream fault.
                AdmissionFailure::QueueFull => {
                    (StatusCode::TOO_MANY_REQUESTS, "admission_queue_full")
                }
                AdmissionFailure::TimedOut => {
                    (StatusCode::SERVICE_UNAVAILABLE, "admission_timeout")
                }
            };
            let (reason, setting) = match failure {
                AdmissionFailure::QueueFull => (
                    "admission queue full".to_owned(),
                    if execution.placement == "cpu" {
                        "--cpu-max-queued-tasks"
                    } else {
                        "--max-queued-tasks"
                    },
                ),
                AdmissionFailure::TimedOut => (
                    format!("no admission slot within {}s", wait.as_secs()),
                    if execution.placement == "cpu" {
                        "--cpu-max-concurrent-tasks"
                    } else {
                        "--max-concurrent-tasks"
                    },
                ),
            };
            Err(ApiError::new(
                status,
                format!(
                    "server busy: {reason} (task cost {cost} of {budget} units; priority \
                     {priority:?} on the {} backend). Retry after {retry_after}s, or raise {setting}.",
                    execution.placement,
                ),
            )
            .with_code(code)
            .with_retry_after(retry_after))
        }
    }
}

pub(super) fn transition_timeout(execution: &ExecutionTarget) -> ApiError {
    execution.admission.record_transition_timeout();
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        format!(
            "server busy: {} backend transition exceeded the admission-and-transition queue deadline",
            execution.placement
        ),
    )
    .with_retry_after(5)
}

pub(super) async fn run_task(
    State(state): State<PlatformState>,
    Json(input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    let session_id = input.route.session_id.clone();
    let task = super::task_key(input.route.task);
    let priority = input.priority;
    let requested_model = input.route.model.clone();
    let result = super::with_session_cancellation(
        &state,
        session_id.as_deref(),
        Box::pin(run_session_task(State(state.clone()), Json(input))),
    )
    .await;
    let record = task_record(&result, started, task, priority, requested_model.as_deref());
    if record.outcome == "ok" {
        state
            .activity
            .completed(&record.backend, &record.model, record.load_ms);
    }
    state.telemetry.record_task(record).await;
    result
}

/// Usage-ledger row for a finished managed call, successful or not.
fn task_record(
    result: &Result<Json<Value>, ApiError>,
    started: Instant,
    task: &str,
    priority: TaskPriority,
    requested_model: Option<&str>,
) -> super::telemetry::TaskRecord {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let priority = match priority {
        TaskPriority::Interactive => "interactive",
        TaskPriority::Normal => "normal",
        TaskPriority::Background => "background",
    };
    let mut record = super::telemetry::TaskRecord {
        ts: super::telemetry::now_seconds(),
        model: requested_model.unwrap_or("unrouted").to_owned(),
        backend: "none".into(),
        task: task.to_owned(),
        priority: priority.to_owned(),
        outcome: "ok".into(),
        status: 200,
        prompt_tokens: 0,
        output_tokens: 0,
        duration_ms,
        queue_wait_ms: 0,
        load_ms: 0,
        output_tokens_per_second: None,
    };
    match result {
        Ok(Json(value)) => {
            if let Some(model) = value["route"]["selected_model"].as_str() {
                model.clone_into(&mut record.model);
            }
            if let Some(backend) = value["execution"]["placement"].as_str() {
                backend.clone_into(&mut record.backend);
            }
            let metrics = &value["metrics"];
            record.prompt_tokens = metrics["prompt_tokens"].as_u64().unwrap_or(0);
            record.output_tokens = metrics["output_tokens"].as_u64().unwrap_or(0);
            record.load_ms = metrics["load_duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
            record.output_tokens_per_second = metrics["output_tokens_per_second"].as_f64();
            record.queue_wait_ms = value["admission"]["total_wait_ms"].as_u64().unwrap_or(0);
        }
        Err(error) => {
            record.outcome = "error".into();
            record.status = error.status().as_u16();
        }
    }
    record
}

#[allow(clippy::too_many_lines)] // Explicit RAII lifetime across admission, revalidation and forwarding.
async fn run_session_task(
    State(state): State<PlatformState>,
    Json(mut input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    // Function definitions are an execution requirement, not merely an optional payload field.
    // Derive the capability at the server boundary so direct HTTP/NAPI callers cannot accidentally
    // route tool work to a completion-only model.
    if input.tools.is_some() {
        input.route.required_capabilities.insert(Capability::Tools);
    }
    require_active_session(&state, input.route.session_id.as_deref()).await?;
    let batch_items = input_batch_items(&input);
    let models = discover_models(&state).await?;
    let sessions = state.sessions.read().await;
    let mut managed = select_managed_route(
        &state,
        &input.route,
        &models,
        &sessions,
        task_cost_for(input.route.task, batch_items),
    )
    .await?;
    drop(sessions);

    apply_request_options(input.route.task, &mut managed.route, &input.request_options)?;
    let mut context_sizing = context::size_context(&input, &mut managed.route, &managed.model)?;
    if managed.route.resident && context_sizing["mode"] == "prompt_estimate" {
        let loaded = resident_entry(
            &state.client,
            &managed.execution.upstream,
            &managed.route.selected_model,
        )
        .await;
        if let Some(reused) =
            context::reuse_resident_context(&mut managed.route, &managed.model, loaded.as_ref())
        {
            context_sizing["reused_resident_context"] = json!(reused);
        }
    }
    let leave_context_to_ollama =
        context_left_to_ollama(&state, &mut managed, &context_sizing).await;
    let preflight = memory_kv_preflight(
        &managed.model,
        &managed.route,
        managed.execution.placement,
        &managed.execution.upstream,
    );
    if preflight["refuses"] == true {
        return Err(ApiError::bad_request(
            "model file size alone exceeds the 80% local CPU/unified host budget; select a smaller model or configure an Ollama backend on a separate host",
        ));
    }

    let mut execution_receipt = managed
        .execution_receipt(task_cost_for(input.route.task, batch_items), &state)
        .await;
    execution_receipt["context_sizing"] = context_sizing;
    execution_receipt["model_digest"] = json!(managed.model.digest);
    let resource_model = managed.model;
    let decision = managed.route;
    let execution = managed.execution;
    // From here until the task ends this model counts as busy: eviction planning leaves it
    // alone, and its recent demand makes it more expensive to unload later.
    let _active = state
        .activity
        .begin(execution.placement, &decision.selected_model);
    // Fail fast while this backend's circuit is open instead of queueing for a dead upstream.
    // The guard lets exactly one probe through after the cooldown.
    let breaker_probe = match state.breakers.check(execution.placement) {
        Ok(probe) => probe,
        Err(remaining) => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "{} backend circuit open after repeated upstream failures; retry in {}s",
                    execution.placement,
                    remaining.as_secs().max(1)
                ),
            )
            .with_code("upstream_circuit_open")
            .with_retry_after(remaining.as_secs().max(1)));
        }
    };
    let immediate_unload = requests_immediate_unload(input.keep_alive.as_deref());
    // `keep_alive:0` makes Ollama unload before its response reaches FreeLlama, so `/api/ps`
    // cannot prove where the work ran. Hold the runner briefly, observe it, then issue and verify
    // an explicit unload before returning. The caller still receives immediate-unload semantics.
    let keep_alive = if immediate_unload {
        json!("30s")
    } else {
        normalize_keep_alive(input.keep_alive.take())
    };
    let (path, mut body) = build_managed_request(&mut input, &decision, &keep_alive)?;
    apply_execution_options(&mut body, &execution);
    if let Some(default_context) = leave_context_to_ollama {
        // Ollama's own default covers this request: leave `num_ctx` unset so the runner stays
        // in Ollama's automatic mode (shared with raw clients, OOM shrink-and-retry intact).
        if let Some(options) = body["options"].as_object_mut() {
            options.remove("num_ctx");
        }
        execution_receipt["context_sizing"]["num_ctx_sent"] = json!(false);
        execution_receipt["context_sizing"]["ollama_default_context"] = default_context;
    }
    execution_receipt["runtime_options"] = body["options"].clone();
    // Slot first, THEN the transition lock — in both branches. The order matters: if the
    // non-resident path took the write lock before its slot while resident tasks held slots and
    // waited on the read lock, the two would deadlock. One consistent order removes that entirely.
    let queue_wait = input.max_wait_seconds.map_or_else(
        || state.tunables().max_queue_wait(),
        |seconds| Duration::from_secs(seconds.max(1)).min(state.tunables().max_queue_wait()),
    );
    let queue_deadline = tokio::time::Instant::now() + queue_wait;
    let (mut slot, mut cost, mut queue_wait_ms) = admit(
        &state,
        &execution,
        decision.task,
        input.priority,
        batch_items,
        queue_deadline,
    )
    .await?;

    // Wait for host memory while holding the admission slot only briefly. A cold load that must
    // wait for memory used to keep its slot for the whole queue deadline, blocking resident
    // requests that need no new memory at all (head-of-line blocking). After the short bound it
    // gives the slot back, waits for memory, then queues for a slot again with memory reserved.
    let resource_started = Instant::now();
    let quick_deadline =
        (tokio::time::Instant::now() + SLOT_HELD_RESOURCE_WAIT).min(queue_deadline);
    let reserved = tokio::time::timeout_at(
        quick_deadline,
        reserve_task_resources(
            &state,
            &execution,
            &resource_model,
            &decision,
            quick_deadline,
            None,
        ),
    )
    .await;
    let (resource_permit, footprint) = match reserved {
        Ok(Ok(reserved)) => reserved,
        Ok(Err(error)) if !error.is_resource_wait() => return Err(error),
        _ if tokio::time::Instant::now() >= queue_deadline => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "resource admission deadline exceeded",
            )
            .with_resource_deadline(
                "initial_reservation",
                resource_started.elapsed().as_millis(),
            ));
        }
        _ => {
            drop(slot);
            let reserved = tokio::time::timeout_at(
                queue_deadline,
                reserve_task_resources(
                    &state,
                    &execution,
                    &resource_model,
                    &decision,
                    queue_deadline,
                    None,
                ),
            )
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "resource admission deadline exceeded",
                )
                .with_resource_deadline(
                    "initial_reservation",
                    resource_started.elapsed().as_millis(),
                )
            })??;
            let (readmitted, readmitted_cost, requeued_ms) = admit(
                &state,
                &execution,
                decision.task,
                input.priority,
                batch_items,
                queue_deadline,
            )
            .await?;
            slot = readmitted;
            cost = readmitted_cost;
            queue_wait_ms = queue_wait_ms.saturating_add(requeued_ms);
            reserved
        }
    };
    let mut resource_wait_ms = resource_started.elapsed().as_millis();
    execution_receipt["resource_admission"] = json!(resource_permit.receipt);
    execution_receipt["memory_reservation"] = footprint;

    // Residency was discovered before admission and can be stale by the time this request reaches
    // the transition lock. Recheck while holding the read side: if the selected runner is still
    // resident, that lock prevents a managed writer from transitioning it during execution. A
    // stale or unavailable snapshot falls back to the exclusive side before the request is sent.
    let transition_started = Instant::now();
    let transition = tokio::time::timeout_at(queue_deadline, async {
        if decision.resident {
            let shared = Arc::clone(&execution.transition).read_owned().await;
            if model_is_resident(&state.client, &execution.upstream, &decision.selected_model).await
            {
                TransitionPermit::Shared(shared)
            } else {
                drop(shared);
                TransitionPermit::Exclusive(Arc::clone(&execution.transition).write_owned().await)
            }
        } else {
            TransitionPermit::Exclusive(Arc::clone(&execution.transition).write_owned().await)
        }
    })
    .await
    .map_err(|_| transition_timeout(&execution))?;
    let transition_wait_ms = transition_started.elapsed().as_millis();
    let recheck_started = Instant::now();
    let (resource_recheck, footprint) = tokio::time::timeout_at(
        queue_deadline,
        reserve_task_resources(
            &state,
            &execution,
            &resource_model,
            &decision,
            queue_deadline,
            Some(resource_permit.receipt.reserved_bytes),
        ),
    )
    .await
    .map_err(|_| {
        transition_timeout(&execution).with_resource_deadline(
            "reservation_revalidation",
            recheck_started.elapsed().as_millis(),
        )
    })??;
    resource_wait_ms = resource_wait_ms.saturating_add(recheck_started.elapsed().as_millis());
    execution_receipt["resource_revalidation"] = json!(resource_recheck.receipt);
    let previous = execution_receipt["memory_reservation"].clone();
    execution_receipt["memory_reservation"] = footprint;
    for key in ["evicted_idle_models", "eviction"] {
        if let Some(value) = previous.get(key) {
            execution_receipt["memory_reservation"][key] = value.clone();
        }
    }
    let admission_mode = transition.admission_mode();
    let selected_model = decision.selected_model.clone();
    let session_id = input.route.session_id.clone();
    let mut permits = [resource_permit, resource_recheck];
    let forward = forward_managed_task(
        &state,
        decision,
        &execution,
        execution_receipt,
        path,
        body,
        admission_mode,
        slot,
        queue_wait_ms,
        transition_wait_ms,
        resource_wait_ms,
        cost,
        immediate_unload,
        breaker_probe,
    );
    // On unified memory a loaded runner's weights are wired and visible in OS telemetry, so the
    // reservation that covered the load is returned as soon as the runner is resident instead of
    // being counted a second time until the response ends. (With mmap'd weights on Linux the
    // page cache still reads as available, so there the reservation is held to the end.)
    let result =
        if host_has_unified_memory() && permits.iter().any(|permit| permit.reserved_bytes() > 0) {
            let client = state.client.clone();
            let upstream = execution.upstream.clone();
            let watch = async {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if model_is_resident(&client, &upstream, &selected_model).await {
                        permits
                            .iter_mut()
                            .for_each(resources::ResourcePermit::release);
                        break;
                    }
                }
                std::future::pending::<()>().await;
            };
            tokio::select! {
                result = forward => result,
                () = watch => unreachable!("the residency watch never completes"),
            }
        } else {
            forward.await
        };
    drop(transition);
    drop(permits);

    // Affinity means the last model that successfully executed for the session. An upstream error
    // must not pin a model the caller never received a successful result from.
    if result.is_ok()
        && let Some(id) = session_id.as_deref()
    {
        state.sessions.write().await.bind(id, &selected_model);
    }
    result
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
    if tunables.context_mode != super::ContextMode::OllamaDefault
        || context_sizing["mode"] != "prompt_estimate"
        || context_sizing.get("reused_resident_context").is_some()
    {
        return None;
    }
    let sized = managed.route.options["num_ctx"].as_u64()?;
    let (default, source) = super::monitor::ollama_default_context(
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

/// Feed the circuit breaker with one upstream call's outcome.
fn record_upstream_outcome(
    state: &PlatformState,
    execution: &ExecutionTarget,
    probe: Option<super::runtime::BreakerProbe>,
    failed: bool,
) {
    let tunables = state.tunables();
    drop(probe);
    if state.breakers.record(
        execution.placement,
        !failed,
        tunables.breaker_failures,
        tunables.breaker_cooldown(),
    ) {
        state.telemetry.record_breaker_open(execution.placement);
        eprintln!(
            "circuit breaker: {} backend open for {}s after repeated upstream failures",
            execution.placement, tunables.breaker_cooldown_seconds
        );
    }
}

/// Feed adaptive concurrency and the service-time average with one finished upstream call.
async fn observe_completion(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
    failed: bool,
    output_tokens_per_second: Option<f64>,
) {
    if !super::monitor::adaptive_applies(state, execution.placement) {
        return;
    }
    let observation = state.resources.snapshot().await.observation;
    let memory_pressure = matches!(
        observation.memory_pressure,
        Some(resources::MemoryPressure::Warning | resources::MemoryPressure::Critical)
    ) || observation
        .memory_psi_some_avg10
        .is_some_and(|stall| stall >= ADAPTIVE_PSI_THRESHOLD);
    let change = state.adaptive_for(execution.placement).observe(
        &execution.admission,
        &super::runtime::Completion {
            model,
            failed,
            memory_pressure,
            output_tokens_per_second,
        },
    );
    super::runtime::note_limit_change(&state.telemetry, execution.placement, change);
}

/// PSI memory `some avg10` (percent of the last 10s with a task stalled on memory) at which a
/// completion counts as "under pressure" for adaptive concurrency.
const ADAPTIVE_PSI_THRESHOLD: f64 = 10.0;

/// Retry 500/502 (load-model blips), but not 503 busy or 504 timeout. Same as passthrough.
/// Retrying 503 while holding an admission slot — and on a cold load, the exclusive write lock —
/// amplifies the saturation the semaphore exists to shed.
fn retryable_managed_status(status: StatusCode) -> bool {
    proxy::retryable_upstream_status(status)
}

pub(super) async fn intent_memory_requirement(
    state: &PlatformState,
    target: &ExecutionTarget,
) -> u64 {
    if !upstream_is_loopback(&target.upstream)
        || !(target.placement == "cpu" || host_has_unified_memory())
    {
        return 0;
    }
    // The interpreter is a cold-capable model request too. Reserve its reported file size even
    // when resident, so a runner transition cannot invalidate a zero-byte reservation.
    get_json(&state.client, &target.upstream, "/api/tags")
        .await
        .ok()
        .and_then(|catalog| catalog["models"].as_array().cloned())
        .and_then(|models| {
            models.into_iter().find(|model| {
                model["name"] == state.intent_model || model["model"] == state.intent_model
            })
        })
        .and_then(|model| model["size"].as_u64())
        .unwrap_or(0)
}

pub(super) async fn model_memory_requirement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
) -> Value {
    let loopback = upstream_is_loopback(&execution.upstream);
    let host_relevant = loopback && (execution.placement == "cpu" || host_has_unified_memory());
    // A discrete GPU holds the model in VRAM, but whatever does not fit spills into system RAM
    // ("mixed" placement), which used to be reserved as zero. Estimate the full footprint, then
    // charge host memory only for the part the GPU's current free VRAM cannot hold.
    let discrete_gpu = loopback && !host_relevant;
    let current = if (host_relevant || discrete_gpu) && model.digest.is_some() {
        resident_entry(&state.client, &execution.upstream, &model.name).await
    } else {
        None
    };
    let context = decision.options["num_ctx"].as_u64().unwrap_or(0);
    let mut footprint = state.footprints.lock().await.requirement(
        &execution.upstream,
        model,
        context,
        current.as_ref(),
        host_relevant || discrete_gpu,
    );
    if discrete_gpu {
        let full = footprint["required_available_bytes"].as_u64().unwrap_or(0);
        if full > 0 {
            let observation = state.resources.snapshot().await.observation;
            footprint["full_estimate_bytes"] = json!(full);
            if let Some(vram_free) = observation.gpu_memory_free_bytes {
                footprint["gpu_memory_free_bytes"] = json!(vram_free);
                footprint["gpu_telemetry_source"] = json!(observation.gpu_telemetry_source);
                footprint["required_available_bytes"] = json!(full.saturating_sub(vram_free));
                footprint["source"] = json!("discrete_gpu_spill_estimate");
            } else {
                footprint["required_available_bytes"] = json!(0);
                footprint["source"] = json!("discrete_gpu_without_vram_telemetry");
            }
        }
    }
    footprint
}

/// Unload idle runners when the load behind `footprint` would not fit, and return the footprint
/// and byte requirement re-measured afterwards.
async fn make_room(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
    mut footprint: Value,
    mut bytes: u64,
) -> (Value, u64) {
    let mut evictions = Vec::new();
    // Discrete GPU: make room in VRAM before Ollama must. Ollama would either evict by its
    // own least-recently-used order (possibly a model with work queued here) or spill layers
    // to the CPU; FreeLlama knows which runners are idle and cheap to reload.
    let vram_shortfall = footprint["full_estimate_bytes"]
        .as_u64()
        .zip(footprint["gpu_memory_free_bytes"].as_u64())
        .map_or(0, |(full, free)| full.saturating_sub(free));
    if vram_shortfall > 0
        && let Some(receipt) = evict_idle_models(
            state,
            execution,
            &decision.selected_model,
            residency::Freed::Vram,
            vram_shortfall,
        )
        .await
    {
        evictions.push(receipt);
        state.resources.invalidate().await;
        footprint = model_memory_requirement(state, execution, model, decision).await;
        bytes = footprint["required_available_bytes"].as_u64().unwrap_or(0);
    }
    if bytes > 0 {
        let snapshot = state.resources.snapshot().await;
        // A host already holding for low memory recovers only above the resume reserve, and
        // idle runners are often exactly what holds that memory, so both cases evict.
        let assessment = snapshot.assess_capacity(bytes, snapshot.holding);
        let memory_hold = snapshot.reasons.iter().any(|reason| {
            matches!(
                reason,
                resources::PressureReason::LowAvailableMemory
                    | resources::PressureReason::OsMemoryPressure
                    | resources::PressureReason::ActiveSwapping
            )
        });
        let evict = match assessment.denial_reason {
            Some(resources::CapacityDenialReason::InsufficientCapacity) => true,
            Some(resources::CapacityDenialReason::HostPressure) => memory_hold,
            _ => false,
        };
        if evict {
            let shortfall = assessment
                .required_with_reserve_bytes
                .unwrap_or(u64::MAX)
                .saturating_sub(assessment.effective_available_bytes.unwrap_or(0));
            let freed = if footprint["source"] == "discrete_gpu_spill_estimate" {
                residency::Freed::HostSpill
            } else {
                residency::Freed::Total
            };
            if let Some(receipt) =
                evict_idle_models(state, execution, &decision.selected_model, freed, shortfall)
                    .await
            {
                evictions.push(receipt);
                // The cached sample predates the unload; judge the new footprint fresh.
                state.resources.invalidate().await;
                footprint = model_memory_requirement(state, execution, model, decision).await;
                bytes = footprint["required_available_bytes"].as_u64().unwrap_or(0);
            }
        }
    }
    if !evictions.is_empty() {
        footprint["evicted_idle_models"] = json!(
            evictions
                .iter()
                .flat_map(|receipt| receipt["unloaded_ok"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default())
                .collect::<Vec<_>>()
        );
        footprint["eviction"] = json!(evictions);
    }
    (footprint, bytes)
}

async fn reserve_task_resources(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
    deadline: tokio::time::Instant,
    already_reserved: Option<u64>,
) -> Result<(resources::ResourcePermit, Value), ApiError> {
    let mut footprint = model_memory_requirement(state, execution, model, decision).await;
    let mut bytes = footprint["required_available_bytes"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(already_reserved.unwrap_or(0));
    if already_reserved.is_none() && state.tunables().evict_idle_models {
        (footprint, bytes) = make_room(state, execution, model, decision, footprint, bytes).await;
    }
    let resident = footprint["source"] == "matching_resident_context";
    if already_reserved.is_some() {
        // Refresh under the transition guard, but never wait for pressure recovery while holding
        // that guard: an unload request may be what makes recovery possible.
        state.resources.snapshot().await;
    }
    let demand = if resident && bytes == 0 {
        resources::ResourceDemand::resident()
    } else {
        resources::ResourceDemand::load(bytes)
    };
    let permit = state
        .resources
        .wait_for_demand(
            &execution.upstream,
            demand,
            if already_reserved.is_some() {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(tokio::time::Instant::now())
            },
        )
        .await
        .map_err(resource_error)?;
    Ok((permit, footprint))
}

/// Unload the cheapest set of idle runners on `execution` that frees about `shortfall` bytes.
///
/// A cold load used to wait for memory that only idle resident models were holding; `FreeLlama`
/// never unloaded them, so the request held its slot and then failed with 503. The victims come
/// from `residency::plan`: never the target, a pinned or `keep_alive: -1` model, or one with
/// tasks queued or running here; among the rest, the set whose loss costs least (measured reload
/// time, recent demand, operator weight). `keep_alive: 0` is safe for a runner that is still
/// serving a raw client: Ollama unloads it once those requests finish.
async fn evict_idle_models(
    state: &PlatformState,
    execution: &ExecutionTarget,
    keep: &str,
    freed: residency::Freed,
    shortfall: u64,
) -> Option<Value> {
    let ps = get_json(&state.client, &execution.upstream, "/api/ps")
        .await
        .ok()?;
    state
        .footprints
        .lock()
        .await
        .observe_resident(&execution.upstream, &ps);
    let tunables = state.tunables();
    let plan = residency::plan(
        &ps,
        &state.activity,
        &residency::PlanInputs {
            backend: execution.placement,
            keep,
            pinned: &tunables.pinned_models,
            weights: &tunables.eviction_costs,
            freed,
            shortfall,
        },
    );
    let mut evicted = Vec::new();
    for victim in &plan.victims {
        let body = json!({"model": victim.name, "keep_alive": 0, "stream": false});
        if post_json_with_retries(state, &execution.upstream, "/api/generate", &body)
            .await
            .is_ok()
        {
            evicted.push(victim.name.clone());
        }
    }
    // Give Ollama a moment to release the runners before capacity is re-read.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !evicted.is_empty() && Instant::now() < deadline {
        let mut still_loaded = false;
        for name in &evicted {
            if model_is_resident(&state.client, &execution.upstream, name).await {
                still_loaded = true;
                break;
            }
        }
        if !still_loaded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    state
        .telemetry
        .record_evictions(execution.placement, evicted.len());
    let mut receipt = plan.receipt();
    receipt["backend"] = json!(execution.placement);
    receipt["for_model"] = json!(keep);
    receipt["memory"] = json!(match freed {
        residency::Freed::Total => "host",
        residency::Freed::Vram => "vram",
        residency::Freed::HostSpill => "host_spill",
    });
    receipt["unloaded_ok"] = json!(evicted);
    receipt["at"] = json!(super::telemetry::now_seconds());
    *state
        .last_eviction
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(receipt.clone());
    (!plan.victims.is_empty()).then_some(receipt)
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
            Err(error) if more_attempts && error.is_connect() && !error.is_timeout() => {
                eprintln!("managed task retry attempt={attempt} error={error:#} path={path}");
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Err(error) => return Err(ApiError::upstream(error)),
        }
    }
}

async fn validate_upstream_completion(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
    path: &str,
    status: StatusCode,
    value: Value,
    immediate_unload: bool,
) -> Result<Value, ApiError> {
    let incomplete = path == "/api/chat" && value["done"] == false;
    let reported_error = value.get("error").is_some_and(|error| !error.is_null());
    if !status.is_success() || incomplete || reported_error {
        // A completed HTTP exchange can still contain an incomplete generation or an error.
        // Do not train feedback or bind affinity. Honor this request's immediate-unload contract,
        // but never replay the uncertain generation to manufacture a completed response.
        let lifecycle = if immediate_unload {
            Some(unload_after_observation(state, execution, model).await)
        } else {
            None
        };
        let code = if incomplete {
            "upstream_incomplete_response"
        } else {
            "upstream_error"
        };
        let message = if incomplete {
            "Ollama returned done:false for a non-streaming managed request".to_owned()
        } else {
            value["error"]
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        };
        return Err(ApiError::new(
            if status.is_success() {
                StatusCode::BAD_GATEWAY
            } else {
                status
            },
            message,
        )
        .with_upstream_response(code, value, lifecycle));
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn forward_managed_task(
    state: &PlatformState,
    decision: RouteDecision,
    execution: &ExecutionTarget,
    mut execution_receipt: Value,
    path: &str,
    body: Value,
    admission_mode: &str,
    // This permit remains owned until completion, including validation and requested unload.
    slot: AdmissionPermit,
    queue_wait_ms: u128,
    transition_wait_ms: u128,
    resource_wait_ms: u128,
    cost: u32,
    immediate_unload: bool,
    breaker_probe: Option<super::runtime::BreakerProbe>,
) -> Result<Json<Value>, ApiError> {
    let posted = post_json_with_retries(state, &execution.upstream, path, &body).await;
    let upstream_failed = match &posted {
        Err(_) => true,
        Ok((status, _)) => matches!(status.as_u16(), 500 | 502 | 504),
    };
    record_upstream_outcome(state, execution, breaker_probe, upstream_failed);
    if upstream_failed {
        observe_completion(state, execution, &decision.selected_model, true, None).await;
    }
    let (status, value) = posted?;
    let value = validate_upstream_completion(
        state,
        execution,
        &decision.selected_model,
        path,
        status,
        value,
        immediate_unload,
    )
    .await?;
    let metrics = runtime_metrics(&value);
    observe_completion(
        state,
        execution,
        &decision.selected_model,
        false,
        metrics["output_tokens_per_second"].as_f64(),
    )
    .await;
    let placement = observe_physical_placement(state, execution, &decision.selected_model).await;
    state.footprints.lock().await.observe(
        &execution.upstream,
        execution_receipt["model_digest"].as_str(),
        &placement,
    );
    let feedback_accepted = placement["status"] == "verified";
    execution_receipt["observation"] = placement;
    let slots_total = execution.admission.total();
    // Report throttling rather than hiding it. A caller that fans out embeddings needs to know it
    // is queueing here — otherwise the only symptom is latency it cannot attribute.
    let slots_available = execution.admission.available();
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
            if let Some(path) = state.feedback_file.clone() {
                // A synchronous fsync-and-rename on a runtime worker stalls every task sharing it.
                let written =
                    tokio::task::spawn_blocking(move || persist_feedback(&path, &snapshot))
                        .await
                        .unwrap_or_else(|error| {
                            Err(anyhow::anyhow!("feedback writer panicked: {error}"))
                        });
                match written {
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
            "transition_wait_ms": transition_wait_ms,
            "resource_wait_ms": resource_wait_ms,
            "total_wait_ms": queue_wait_ms.saturating_add(transition_wait_ms).saturating_add(resource_wait_ms),
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
            "error": error.body.error,
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
    let mut observation = physical_placement_observation(
        execution.placement,
        running.get("size").and_then(Value::as_u64),
        running.get("size_vram").and_then(Value::as_u64),
    );
    observation["context_length"] = running
        .get("context_length")
        .cloned()
        .unwrap_or(Value::Null);
    observation["digest"] = running.get("digest").cloned().unwrap_or(Value::Null);
    observation
}

/// The `/api/ps` entry for `model` on `upstream`, if it is loaded.
async fn resident_entry(client: &Client, upstream: &str, model: &str) -> Option<Value> {
    get_json(client, upstream, "/api/ps")
        .await
        .ok()?
        .get("models")?
        .as_array()?
        .iter()
        .find(|entry| {
            entry
                .get("name")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                == Some(model)
        })
        .cloned()
}

async fn model_is_resident(client: &Client, upstream: &str, model: &str) -> bool {
    get_json(client, upstream, "/api/ps")
        .await
        .ok()
        .and_then(|ps| ps.get("models").and_then(Value::as_array).cloned())
        .is_some_and(|models| {
            models.iter().any(|entry| {
                entry
                    .get("name")
                    .or_else(|| entry.get("model"))
                    .and_then(Value::as_str)
                    == Some(model)
            })
        })
}

pub(super) fn physical_placement_observation(
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
pub(super) fn feedback_work_unit_ns(task: TaskKind, response: &Value) -> Option<u64> {
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
    let cached_count = response
        .get("prompt_eval_cached_count")
        .and_then(Value::as_u64)
        .filter(|cached| prompt_count.is_some_and(|total| *cached <= total));
    let uncached_count = prompt_count
        .zip(cached_count)
        .map(|(total, cached)| total - cached);
    let prompt_duration = response.get("prompt_eval_duration").and_then(Value::as_u64);
    let output_count = response.get("eval_count").and_then(Value::as_u64);
    let output_duration = response.get("eval_duration").and_then(Value::as_u64);
    json!({
        "total_duration_ns": response.get("total_duration").and_then(Value::as_u64),
        "load_duration_ns": response.get("load_duration").and_then(Value::as_u64),
        "prompt_tokens": prompt_count,
        "cached_prompt_tokens": cached_count,
        "uncached_prompt_tokens": uncached_count,
        "prompt_duration_ns": prompt_duration,
        "prompt_tokens_per_second": tokens_per_second(uncached_count, prompt_duration),
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
pub(super) fn task_cost(task: TaskKind) -> u32 {
    match task {
        // No autoregressive decode; batching remains the preferred throughput path.
        TaskKind::Embedding => 1,
        // Image payload and multimodal prefill in addition to generation.
        TaskKind::Vision => 4,
        _ => 2,
    }
}

/// Charge an embedding batch for its actual cardinality. Ollama executes a batched `/api/embed`
/// request as more work than a single string even though it remains materially cheaper than N
/// independent HTTP calls. The cap prevents an input array from reserving more than the backend
/// can ever supply; `admit` applies the final backend-specific cap.
fn task_cost_for(task: TaskKind, batch_items: usize) -> u32 {
    if !matches!(task, TaskKind::Embedding) {
        return task_cost(task);
    }
    let items = u32::try_from(batch_items.max(1)).unwrap_or(u32::MAX);
    // One unit covers up to four compact embedding inputs; each additional group of four adds a
    // unit. This is an intentionally transparent queueing weight, not an invented VRAM estimate.
    items.saturating_add(3) / 4
}

fn input_batch_items(input: &TaskInput) -> usize {
    input
        .input
        .as_ref()
        .and_then(Value::as_array)
        .map_or(1, Vec::len)
        .max(1)
}

/// Advisory F16 estimate and a coarse local model-size budget, not Ollama's live loader check.
/// Cache precision and runner parallelism belong to the selected Ollama process, so reading the
/// gateway's environment cannot establish either. An F16 estimate can exceed actual quantized
/// cache usage and must not be presented as a known minimum or used to reject that configuration.
fn memory_kv_preflight(
    model: &CatalogModel,
    route: &RouteDecision,
    placement: &str,
    upstream: &str,
) -> Value {
    let requested_context = route
        .options
        .get("num_ctx")
        .and_then(Value::as_u64)
        .or(model.advertised_context);
    let host_relevant =
        upstream_is_loopback(upstream) && (placement == "cpu" || host_has_unified_memory());
    memory_kv_preflight_with_memory(
        model,
        requested_context,
        host_relevant,
        host_total_memory_bytes(),
    )
}

pub(super) fn upstream_is_loopback(upstream: &str) -> bool {
    reqwest::Url::parse(upstream)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

pub(super) fn memory_kv_preflight_with_memory(
    model: &CatalogModel,
    requested_context: Option<u64>,
    host_relevant: bool,
    host_memory_bytes: Option<u64>,
) -> Value {
    let kv_cache_bytes = model
        .kv_cache_bytes_per_token_f16
        .zip(requested_context)
        .and_then(|(per_token, context)| per_token.checked_mul(context));
    let model_plus_kv_bytes = kv_cache_bytes.and_then(|kv| model.size.checked_add(kv));
    let host_budget = host_memory_bytes.map(|total| total / 5 * 4);
    let refuses = host_relevant && host_budget.is_some_and(|budget| model.size > budget);
    let estimate_exceeds_budget = host_relevant
        && model_plus_kv_bytes
            .zip(host_budget)
            .is_some_and(|(needed, budget)| needed > budget);
    let status = if refuses {
        "refuse_model_file_exceeds_host_budget"
    } else if estimate_exceeds_budget {
        "f16_estimate_exceeds_host_budget"
    } else if kv_cache_bytes.is_some() {
        "estimated_f16_single_sequence"
    } else {
        "unknown_model_metadata"
    };
    json!({
        "status": status,
        "refuses": refuses,
        "known_model_bytes": model.size,
        "requested_context_tokens": requested_context,
        "kv_cache_bytes_f16_estimate": kv_cache_bytes,
        "model_plus_kv_bytes_f16_estimate": model_plus_kv_bytes,
        "host_memory_bytes": host_memory_bytes,
        "host_budget_bytes": host_budget,
        "host_memory_relevant": host_relevant,
        "threshold": "model_file_only_80_percent_of_total_host_memory_when_loopback_cpu_or_unified",
        "assumptions": "unquantized F16, one sequence, uniform full attention, no padding or runner overhead; loopback assumed local",
        "upstream_parallelism": "unknown",
        "upstream_kv_cache_type": "unknown",
        "live_available_memory_bytes": null,
        "authority": "Ollama owns live free-memory, runner graph, cache-type, and final load admission",
    })
}
