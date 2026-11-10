//! HTTP handlers that list, read, cancel and remove deferred jobs and recovered receipts.

use std::sync::{Arc, Mutex};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::platform::{PlatformState, error::ApiError};

use super::registry::{journal, lookup, snapshot};
use super::{Job, JobStatus};

pub(in crate::platform) async fn list(
    State(state): State<PlatformState>,
) -> Result<Json<Value>, ApiError> {
    let mut current = snapshot(&state.jobs);
    if let Some(journal) = journal(&state) {
        let entries = current["jobs"].as_array_mut().expect("snapshot jobs array");
        for mut recovered in journal.list().await? {
            if !entries.iter().any(|job| job["id"] == recovered["id"]) {
                if let Some(object) = recovered.as_object_mut() {
                    object.remove("result");
                    object.remove("error");
                }
                entries.push(recovered);
            }
        }
    }
    Ok(Json(current))
}

/// The longest a job read waits for a change: below every client's control timeout.
const MAX_LONG_POLL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Default, Deserialize)]
pub(in crate::platform) struct GetQuery {
    /// Wait up to this long (capped at five seconds) for the job to change before answering.
    wait_ms: Option<u64>,
}

pub(in crate::platform) async fn get(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    Query(query): Query<GetQuery>,
) -> Result<Json<Value>, ApiError> {
    let job = match lookup(&state.jobs, &id) {
        Ok(job) => job,
        Err(error) => {
            if let Some(journal) = journal(&state)
                && let Some(receipt) = journal.get(id).await?
            {
                return Ok(Json(json!({"job":receipt})));
            }
            return Err(error);
        }
    };
    let Some(wait) = query
        .wait_ms
        .map(|ms| std::time::Duration::from_millis(ms).min(MAX_LONG_POLL))
    else {
        let receipt = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .receipt(true);
        return Ok(Json(json!({"job":receipt})));
    };
    // Register for the next change before reading the status, so a change between the read
    // and the wait still wakes this request.
    let changed = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .changed
        .clone();
    let notified = changed.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let terminal = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .status
        .terminal();
    if !terminal {
        let _ = tokio::time::timeout(wait, notified).await;
    }
    let receipt = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt(true);
    // `long_poll` tells a client this serve waits, so it need not sleep between reads; an
    // older serve ignores `wait_ms` and omits it.
    Ok(Json(json!({"job":receipt,"long_poll":true})))
}

pub(in crate::platform) async fn cancel(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if lookup(&state.jobs, &id).is_err() {
        // Recovery never claims to cancel a worker owned by a different process.
        return get(State(state), Path(id), Query(GetQuery::default())).await;
    }
    let job = cancel_job(&state, &id).await?;
    let receipt = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt(true);
    Ok(Json(json!({"job":receipt})))
}

async fn cancel_job(state: &PlatformState, id: &str) -> Result<Arc<Mutex<Job>>, ApiError> {
    // Keep the handle alive across completion even if bounded retention evicts its entry.
    let job = lookup(&state.jobs, id)?;
    let mut done = {
        let mut current = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.status.terminal() && current.persistence_state != "pending" {
            drop(current);
            return Ok(job);
        }
        if !current.status.terminal() && !current.completion_committed {
            current.status = JobStatus::Cancelling;
            current.reason = "cancellation_requested";
            let _ = current.cancel.send(true);
            current.changed.notify_waiters();
        }
        current.done.clone()
    };
    // A receipt saying cancelled is returned only after the worker has dropped its permits.
    done.wait_for(|finished| *finished)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "job worker unavailable"))?;
    Ok(job)
}

/// Stop only this job, wait for local permits, then hide it and discard its payload. Durable
/// idempotency tombstones remain until expiry, so removal cannot authorize a duplicate run.
pub(in crate::platform) async fn remove(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if lookup(&state.jobs, &id).is_err()
        && let Some(journal) = journal(&state)
        && let Some(receipt) = journal.get(id.clone()).await?
    {
        if receipt["reason"] == "execution_outcome_unknown" {
            return Err(ApiError::new(StatusCode::CONFLICT,"cannot remove uncertain work owned by another or interrupted worker; wait for retention expiry").with_code("task_job_outcome_unknown"));
        }
        journal.remove(id.clone()).await?;
        return Ok(Json(
            json!({"id":id,"removed":true,"status":receipt["status"],"scope":"database"}),
        ));
    }
    let job = cancel_job(&state, &id).await?;
    let journal = journal(&state);
    if let Some(journal) = &journal {
        journal.remove(id.clone()).await?;
    }
    let status = {
        let mut job = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.result = None;
        job.error = None;
        job.status
    };
    state
        .jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .remove(&id);
    Ok(Json(
        json!({"id":id,"removed":true,"status":status,"scope":if journal.is_some() { "database" } else { "process_memory" }}),
    ))
}
