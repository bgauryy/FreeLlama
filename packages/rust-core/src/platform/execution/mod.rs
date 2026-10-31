//! Managed execution: route selection, capacity reservation, forwarding, and evidence receipts.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::admission::AdmissionPool;
use super::error::ApiError;
use super::{Backend, PlatformState, TaskInput};

mod admit;
mod feedback;
mod forward;
mod memory;
mod observe;
mod pipeline;
mod request;
mod select;
mod upstream;

pub(super) use admit::{admit, transition_timeout};
#[cfg(test)]
pub(super) use feedback::feedback_work_unit_ns;
pub use feedback::runtime_metrics;
use feedback::{record_completion, task_record};
use forward::{AdmissionTimings, ForwardRequest, forward_managed_task};
#[cfg(test)]
pub(super) use memory::memory_kv_preflight_with_memory;
pub(super) use memory::{
    evict_idle_models, host_memory_relevant, make_room, model_memory_requirement, requested_context,
};
use memory::{memory_kv_preflight, reserve_task_resources};
pub(super) use observe::physical_placement_observation;
use observe::{model_is_resident, resident_entry};
use pipeline::run_session_task;
pub(super) use request::apply_execution_options;
pub(super) use request::requests_immediate_unload;
use request::{
    apply_request_options, build_managed_request, input_batch_items, task_cost_for, task_has_images,
};
pub(super) use select::{ManagedDecision, select_managed_route};

/// How long a request may wait for host memory while holding its admission slot.
const SLOT_HELD_RESOURCE_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct ExecutionTarget {
    pub(super) placement: Backend,
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
            placement: Backend::Cpu,
            upstream: upstream.clone(),
            transition: Arc::clone(&state.cpu_managed_execution),
            admission: state.cpu_admission.clone(),
        };
    }
    primary_target(state)
}

/// The backend name (`gpu` or `cpu`) a model runs on.
pub(super) fn execution_target_name(state: &PlatformState, model: &str) -> &'static str {
    execution_target(state, model).placement.as_str()
}

/// The primary Ollama backend: every model not assigned to the CPU backend, and all raw traffic.
pub(super) fn primary_target(state: &PlatformState) -> ExecutionTarget {
    ExecutionTarget {
        placement: Backend::Gpu,
        upstream: state.upstream.clone(),
        transition: Arc::clone(&state.managed_execution),
        admission: state.gpu_admission.clone(),
    }
}

/// Unload a resident `model` from the backend it runs on once no task is using it, or `None`
/// when it is not resident (the caller then decides whether to probe it). An unload names a
/// model, not a task, so it skips routing and task eligibility: an embedding model unloads
/// without `task: "embedding"`. The exclusive transition side waits for tasks already running
/// on the resident runner, bounded by `wait`.
pub(super) async fn unload_resident_model(
    state: &PlatformState,
    model: &str,
    wait: Duration,
) -> Result<Option<Value>, ApiError> {
    let execution = execution_target(state, model);
    let backend = execution.placement.as_str();
    if !model_is_resident(&state.client, &execution.upstream, model).await {
        return Ok(None);
    }
    let started = Instant::now();
    let _transition = tokio::time::timeout(wait, Arc::clone(&execution.transition).write_owned())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{model} is still serving tasks; retry the unload when they finish"),
            )
            .with_code("unload_wait_exceeded")
        })?;
    let (status, body) = observe::request_unload(state, &execution.upstream, model).await?;
    if !status.is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("Ollama refused to unload {model}: {body}"),
        )
        .with_code("unload_refused"));
    }
    // Ollama answers before the runner has exited; confirm from /api/ps.
    let confirm_by = Instant::now() + Duration::from_secs(5);
    let mut resident = model_is_resident(&state.client, &execution.upstream, model).await;
    while resident && Instant::now() < confirm_by {
        tokio::time::sleep(Duration::from_millis(200)).await;
        resident = model_is_resident(&state.client, &execution.upstream, model).await;
    }
    let unload_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if !resident {
        state.audit.record(&super::audit::Entry {
            backend,
            model,
            event: "unload",
            trigger: "caller_unload",
            duration_ms: Some(unload_ms),
            bytes: None,
        });
    }
    Ok(Some(serde_json::json!({
        "model": model,
        "backend": backend,
        "unloaded": !resident,
        "status": if resident { "still_resident" } else { "unloaded" },
        "unload_ms": unload_ms,
    })))
}

pub(super) async fn run_task(
    State(state): State<PlatformState>,
    caller: Option<axum::Extension<crate::proxy::Caller>>,
    input: Result<Json<TaskInput>, axum::extract::rejection::JsonRejection>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let Json(mut input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    input.client = caller.map(|axum::Extension(caller)| caller.0.to_string());
    if input.defer {
        return super::jobs::submit(state, input)
            .await
            .map(|job| (StatusCode::ACCEPTED, job).into_response());
    }
    execute_task(State(state), Json(input))
        .await
        .map(IntoResponse::into_response)
}

pub(super) fn task_deadline(seconds: Option<u64>) -> Result<Duration, ApiError> {
    if seconds == Some(0) {
        return Err(ApiError::bad_request("timeout_seconds must be positive"));
    }
    let ceiling = super::platform_task_timeout();
    Ok(seconds.map_or(ceiling, Duration::from_secs).min(ceiling))
}

pub(super) async fn execute_task(
    State(state): State<PlatformState>,
    Json(mut input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let timeout = input
        .timeout_seconds
        .map(|seconds| task_deadline(Some(seconds)))
        .transpose()?
        .or_else(|| {
            (input.scope_id.is_some() || matches!(input.operation, super::ManagedOperation::Warm))
                .then(super::platform_task_timeout)
        });
    let cancellation = input
        .job_progress
        .as_ref()
        .map(super::jobs::JobProgress::cancellation);
    let deadline = timeout.map(|timeout| {
        input.job_progress.as_ref().map_or_else(
            || tokio::time::Instant::now() + timeout,
            super::jobs::JobProgress::deadline,
        )
    });
    let started = Instant::now();
    let mut scope_lease = None;
    let preparation = state
        .scopes
        .prepare(&mut input, &state.tunables().scopes)
        .map(|lease| scope_lease = lease);
    let operation = input.operation;
    let job_progress = input.job_progress.clone();
    let session_id = input.route.session_id.clone();
    let task = input.route.task.as_str();
    let priority = input.priority;
    let requested_model = input.route.model.clone();
    let client = input.client.clone();
    let scoped = scope_lease.is_some();
    let mut scope_session_cancellation = None;
    let work = async {
        preparation?;
        if scoped && let Some(id) = session_id.as_deref() {
            scope_session_cancellation = state.sessions.read().await.cancellation(id);
        }
        super::with_session_cancellation(
            &state,
            session_id.as_deref(),
            Box::pin(run_session_task(State(state.clone()), Json(input))),
        )
        .await
    };
    let bounded = async {
        if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, work)
                .await
                .unwrap_or_else(|_| {
                    Err(
                        ApiError::new(StatusCode::GATEWAY_TIMEOUT, "task total deadline exceeded")
                            .with_code("task_deadline_exceeded"),
                    )
                })
        } else {
            work.await
        }
    };
    let mut result = if let Some(mut cancellation) = cancellation.clone() {
        tokio::select! {
            biased;
            _ = cancellation.wait_for(|cancelled| *cancelled) => Err(ApiError::task_cancelled()),
            result = bounded => result,
        }
    } else {
        bounded.await
    };
    // Retain measured inference costs even when the final history transaction fails.
    let mut record = task_record(
        &result,
        started,
        task,
        priority,
        requested_model.as_deref(),
        client.as_deref(),
    );
    if let Some(lease) = scope_lease
        && let Ok(Json(value)) = &mut result
        && let Err(error) = commit_scoped_completion(
            &state,
            lease,
            value,
            session_id.as_deref(),
            job_progress.as_ref(),
            deadline,
            scope_session_cancellation.as_ref(),
        )
        .await
    {
        record.outcome = "error".into();
        record.status = error.status().as_u16();
        result = Err(error);
    }
    finish_task(&state, record, started, operation, &mut result);
    result
}

/// Record a finished task, let the preloader learn from work a caller asked for, and add the
/// warm receipt to a warm.
fn finish_task(
    state: &PlatformState,
    record: super::telemetry::TaskRecord,
    started: Instant,
    operation: super::ManagedOperation,
    result: &mut Result<Json<Value>, ApiError>,
) {
    let served = (record.outcome == "ok" && matches!(operation, super::ManagedOperation::Generate))
        .then(|| (record.backend.clone(), record.model.clone()));
    record_completion(
        state,
        record,
        started,
        matches!(operation, super::ManagedOperation::Generate),
    );
    if let Some((backend, model)) = served {
        super::preload::served(state, &backend, &model);
    }
    if matches!(operation, super::ManagedOperation::Warm)
        && let Ok(Json(value)) = result
    {
        value["warm"] = super::warming::completion_receipt(&value["execution"]);
    }
}

async fn commit_scoped_completion(
    state: &PlatformState,
    lease: super::scopes::ScopeLease,
    value: &mut Value,
    session_id: Option<&str>,
    progress: Option<&super::jobs::JobProgress>,
    deadline: Option<tokio::time::Instant>,
    session_cancellation: Option<&tokio::sync::watch::Receiver<bool>>,
) -> Result<(), ApiError> {
    let mut sessions = if let Some(deadline) = deadline {
        tokio::time::timeout_at(deadline, state.sessions.write())
            .await
            .map_err(|_| super::scopes::commit_deadline_exceeded())?
    } else {
        state.sessions.write().await
    };
    if session_cancellation.is_some_and(|signal| *signal.borrow()) {
        return Err(ApiError::session_killed());
    }
    let commit = || lease.commit(value, &state.tunables().scopes, deadline);
    if let Some(progress) = progress {
        progress.commit_result(commit)?;
    } else {
        commit()?;
    }
    if let Some(id) = session_id
        && let Some(model) = value["route"]["selected_model"].as_str()
    {
        sessions.bind(id, model);
    }
    Ok(())
}
