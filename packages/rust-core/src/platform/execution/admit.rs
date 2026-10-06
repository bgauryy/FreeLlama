//! Admission slots for managed tasks and the busy errors they refuse with.

use axum::http::StatusCode;

use super::super::admission::{AdmissionFailure, AdmissionPermit, ResourceWaitGuard};
use super::super::error::ApiError;
use super::super::{Backend, PlatformState, TaskPriority};
use super::ExecutionTarget;

/// Wait for an admission slot sized to the task, or refuse.
///
/// Returns the held permit, the cost charged, and how long the caller queued.
pub(crate) async fn admit(
    state: &PlatformState,
    execution: &ExecutionTarget,
    requested_cost: u32,
    priority: TaskPriority,
    deadline: tokio::time::Instant,
    resource_waiter: Option<ResourceWaitGuard>,
) -> Result<(AdmissionPermit, u32, u128), ApiError> {
    let budget = u32::try_from(execution.admission.total())
        .unwrap_or(u32::MAX)
        .max(1);
    let cost = requested_cost.min(budget).max(1);
    let acquired = if let Some(waiter) = resource_waiter {
        waiter
            .acquire(requested_cost.max(1) as usize, priority, deadline)
            .await
    } else {
        execution
            .admission
            .acquire(requested_cost.max(1) as usize, priority, deadline)
            .await
    };
    match acquired {
        Ok((permit, queued)) => {
            let charged = u32::try_from(permit.units())
                .expect("admission charge cannot exceed the u32 requested cost");
            Ok((permit, charged, queued))
        }
        Err(failure) => Err(admission_error(state, execution, priority, cost, failure)),
    }
}

pub(super) fn admission_error(
    state: &PlatformState,
    execution: &ExecutionTarget,
    priority: TaskPriority,
    cost: u32,
    failure: AdmissionFailure,
) -> ApiError {
    let budget = execution.admission.total();
    let wait = state.tunables().max_queue_wait();
    let retry_after = super::super::runtime::retry_after_seconds(
        execution.admission.queue_depth(),
        execution.admission.total(),
        state.average_task_ms(execution.placement),
    );
    let (status, code) = match failure {
        // A full queue is FreeLlama's own configured cap, not an upstream fault.
        AdmissionFailure::QueueFull => (StatusCode::TOO_MANY_REQUESTS, "admission_queue_full"),
        AdmissionFailure::TimedOut => (StatusCode::SERVICE_UNAVAILABLE, "admission_timeout"),
    };
    let (reason, setting) = match failure {
        AdmissionFailure::QueueFull => (
            "admission queue full".to_owned(),
            if execution.placement == Backend::Cpu {
                "--cpu-max-queued-tasks"
            } else {
                "--max-queued-tasks"
            },
        ),
        AdmissionFailure::TimedOut => (
            format!("no admission slot within {}s", wait.as_secs()),
            if execution.placement == Backend::Cpu {
                "--cpu-max-concurrent-tasks"
            } else {
                "--max-concurrent-tasks"
            },
        ),
    };
    ApiError::new(
        status,
        format!(
            "server busy: {reason} (task cost {cost} of {budget} units; priority \
             {priority:?} on the {} backend). Retry after {retry_after}s, or raise {setting}.",
            execution.placement,
        ),
    )
    .with_code(code)
    .with_retry_after(retry_after)
}

pub(crate) fn transition_timeout(execution: &ExecutionTarget) -> ApiError {
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
