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

pub(super) use admit::{admit, transition_timeout};
#[cfg(test)]
pub(super) use feedback::feedback_work_unit_ns;
pub use feedback::runtime_metrics;
use feedback::{record_completion, task_record};
use forward::{AdmissionTimings, ForwardRequest, forward_managed_task};
#[cfg(test)]
pub(super) use memory::memory_kv_preflight_with_memory;
pub(super) use memory::model_memory_requirement;
use memory::{make_room, memory_kv_preflight, reserve_task_resources};
pub(super) use observe::physical_placement_observation;
use observe::{model_is_resident, resident_entry};
use pipeline::run_session_task;
pub(super) use request::apply_execution_options;
use request::{
    apply_request_options, build_managed_request, input_batch_items, requests_immediate_unload,
    task_cost_for, task_has_images,
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
    ExecutionTarget {
        placement: Backend::Gpu,
        upstream: state.upstream.clone(),
        transition: Arc::clone(&state.managed_execution),
        admission: state.gpu_admission.clone(),
    }
}

pub(super) async fn run_task(
    State(state): State<PlatformState>,
    input: Result<Json<TaskInput>, axum::extract::rejection::JsonRejection>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
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
            (input.scope_id.is_some()
                || matches!(input.operation, super::warming::ManagedOperation::Warm))
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
    let mut record = task_record(&result, started, task, priority, requested_model.as_deref());
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
    record_completion(&state, record, started);
    if matches!(operation, super::warming::ManagedOperation::Warm)
        && let Ok(Json(value)) = &mut result
    {
        value["warm"] = super::warming::completion_receipt(&value["execution"]);
    }
    result
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
            .map_err(|_| scope_commit_deadline())?
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

pub(super) fn scope_commit_deadline() -> ApiError {
    ApiError::new(
        StatusCode::GATEWAY_TIMEOUT,
        "task total deadline exceeded before history commit",
    )
    .with_code("task_deadline_exceeded")
}
