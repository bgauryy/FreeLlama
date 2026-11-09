//! Managed execution: route selection, capacity reservation, forwarding, and evidence receipts.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::admission::AdmissionPool;
use super::error::ApiError;
use super::{Backend, PlatformState, TaskInput};

mod admit;
mod completion;
mod feedback;
mod forward;
mod identity;
mod memory;
pub(super) mod nearest;
mod observe;
mod pipeline;
mod request;
pub(super) mod runner;
mod select;
mod upstream;

pub(super) use admit::{admit, transition_timeout};
#[cfg(test)]
pub(super) use feedback::feedback_work_unit_ns;
pub use feedback::runtime_metrics;
use feedback::{qualified_load_ms, record_completion, task_record};
use forward::{AdmissionTimings, ForwardRequest, forward_managed_task};
#[cfg(test)]
pub(super) use memory::memory_kv_preflight_with_memory;
use memory::{Forecast, memory_kv_preflight, reserve_task_resources};
pub(super) use memory::{
    annotate_ollama_preflight, cold_model_memory_requirement, evict_idle_models,
    host_memory_relevant, make_room, model_memory_requirement, requested_context,
};
pub(super) use observe::physical_placement_observation;
use observe::resident_entry;
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
    /// The backend's load lock: it serializes loads and managed explicit unloads. A resident runner is
    /// protected from eviction by its per-model use lease (`ModelActivity`), never by this lock,
    /// so decoding on one model does not delay another model's load.
    pub(super) transition: Arc<Mutex<()>>,
    pub(super) admission: AdmissionPool,
}

/// How a managed task reaches its runner. Either way the task also holds its model's use lease.
enum TransitionPermit {
    /// A compatible runner is resident: no backend lock is held while it generates.
    Resident,
    /// This task loads or reloads its runner and owns the backend load lock until the runner is
    /// observed ready, so loads queue behind each other and same-model arrivals wait here.
    Loading(OwnedMutexGuard<()>),
}

impl TransitionPermit {
    fn admission_mode(&self) -> &'static str {
        match self {
            Self::Resident => "resident_shared",
            Self::Loading(_guard) => "nonresident_transition_exclusive",
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
            transition: Arc::clone(&state.cpu_load_lock),
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
        transition: Arc::clone(&state.load_lock),
        admission: state.gpu_admission.clone(),
    }
}

/// Unload a resident `model` from the backend it runs on once no task is using it, or `None`
/// when it is not resident (the caller then decides whether to probe it). An unload names a
/// model, not a task, so it skips routing and task eligibility: an embedding model unloads
/// without `task: "embedding"`. The model's lifecycle claim waits for tasks already running on
/// the resident runner, bounded by `wait`; the backend load lock then serializes the unload.
pub(super) async fn unload_resident_model(
    state: &PlatformState,
    model: &str,
    wait: Duration,
) -> Result<Option<Value>, ApiError> {
    let execution = execution_target(state, model);
    let backend = execution.placement.as_str();
    let deadline = tokio::time::Instant::now() + wait;
    // Lifecycle first, then the backend load lock. Arriving tasks wait for this claim before
    // acquiring the load lock, so a claim holder never waits behind a task that waits for it.
    let lifecycle =
        tokio::time::timeout_at(deadline, state.activity.evict_when_idle(backend, model))
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("{model} is still serving tasks; retry the unload when they finish"),
                )
                .with_code("unload_wait_exceeded")
            })?;
    let ps = tokio::time::timeout_at(
        deadline,
        super::get_json(&state.client, &execution.upstream, "/api/ps"),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Ollama residency observation timed out",
        )
        .with_code("unload_observation_unavailable")
    })??;
    if !unload_residency(&ps, model)? {
        return Ok(None);
    }
    let started = Instant::now();
    let transition =
        tokio::time::timeout_at(deadline, Arc::clone(&execution.transition).lock_owned())
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("{model} is still serving tasks; retry the unload when they finish"),
                )
                .with_code("unload_wait_exceeded")
            })?;
    let state = state.clone();
    let model = model.to_owned();
    // Cancellation before dispatch releases the claim. Once dispatched, cleanup owns both
    // leases independently of the caller and finishes within the request/confirmation bounds.
    tokio::spawn(async move {
        let _leases = (lifecycle, transition);
        let outcome = finish_explicit_unload(&state, &execution, &model, started).await;
        state.resources.invalidate().await;
        outcome
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("unload supervisor failed: {error}"),
        )
        .with_code("unload_supervisor_failed")
    })?
}

fn unload_residency(ps: &Value, model: &str) -> Result<bool, ApiError> {
    let models = ps["models"]
        .as_array()
        .filter(|models| {
            models
                .iter()
                .all(|entry| crate::ollama_http::ps_entry_name(entry).is_some())
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Ollama residency observation is unavailable or malformed",
            )
            .with_code("unload_observation_unavailable")
        })?;
    Ok(models
        .iter()
        .any(|entry| crate::ollama_http::ps_entry_name(entry) == Some(model)))
}

async fn finish_explicit_unload(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
    started: Instant,
) -> Result<Option<Value>, ApiError> {
    let backend = execution.placement.as_str();
    let posted = tokio::time::timeout(
        Duration::from_secs(5),
        observe::request_unload(state, &execution.upstream, model),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unload request timed out; upstream outcome is unknown",
        )
        .with_code("unload_outcome_unknown")
    })
    .and_then(std::convert::identity);
    // A failed observation is unknown, not absence. Keep lifecycle ownership through the
    // same bounded confirmation used by idle eviction.
    let confirmation =
        memory::confirm_unloads(&[model.to_owned()], Duration::from_secs(5), || async {
            super::get_json(&state.client, &execution.upstream, "/api/ps")
                .await
                .ok()
        })
        .await;
    let (status, body) = posted?;
    if !status.is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("Ollama refused to unload {model}: {body}"),
        )
        .with_code("unload_refused"));
    }
    let unloaded = !confirmation.confirmed.is_empty();
    let unload_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if unloaded {
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
        "unloaded": unloaded,
        "status": if unloaded { "unloaded" } else if confirmation.observation_failed.is_empty() { "still_resident" } else { "observation_unavailable" },
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
    super::jobs::validate_synchronous(&input)?;
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
            Box::pin(nearest::run(state.clone(), input)),
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
        qualified_load_ms(result),
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

#[cfg(test)]
mod unload_tests {
    use super::*;

    #[test]
    fn unavailable_residency_is_not_reported_as_an_absent_runner() {
        for ps in [
            serde_json::json!({"error":"unavailable"}),
            serde_json::json!({"models":[{}]}),
        ] {
            assert!(unload_residency(&ps, "m:latest").is_err());
        }
        assert!(!unload_residency(&serde_json::json!({"models":[]}), "m:latest").unwrap());
        assert!(
            unload_residency(
                &serde_json::json!({"models":[{"name":"m:latest"}]}),
                "m:latest"
            )
            .unwrap()
        );
    }
}
