//! The worker-side handle of one job: status updates, cancellation and terminal receipts.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::platform::error::ApiError;

use super::registry::fits_retention_limit;
use super::{Job, JobStatus, MAX_RESULT_BYTES};

#[derive(Debug, Clone)]
pub(in crate::platform) struct JobProgress {
    pub(super) job: Arc<Mutex<Job>>,
    pub(super) id: String,
    pub(super) cancellation: watch::Receiver<bool>,
    pub(super) deadline: tokio::time::Instant,
    pub(super) journal: Option<crate::platform::job_journal::JobJournal>,
}

impl JobProgress {
    pub(super) async fn persist_terminal(&self) {
        if let Some(journal) = &self.journal {
            let mut receipt = self
                .job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .receipt(true);
            receipt["persistence"]["state"] = json!("confirmed");
            let outcome = journal.finish(self.id.clone(), receipt).await;
            let mut job = self
                .job
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match outcome {
                Ok(()) => job.persistence_state = "confirmed",
                Err(error) => {
                    job.persistence_state = "failed";
                    job.persistence_error = Some(error.into_batch_result(self.id.clone()));
                }
            }
        }
    }

    pub(in crate::platform) fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancellation.clone()
    }

    pub(in crate::platform) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    pub(in crate::platform) fn update(
        &self,
        status: JobStatus,
        reason: &'static str,
        detail: Option<Value>,
    ) {
        let mut job = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !job.status.terminal() && job.status != JobStatus::Cancelling {
            job.status = status;
            job.reason = reason;
            job.detail = detail;
        }
    }

    pub(in crate::platform) fn route(&self, model: &str, backend: &str) {
        let mut job = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.selected_model = Some(model.into());
        job.backend = Some(backend.into());
    }

    /// Linearize history commit against cancellation. After commit, cancellation waits for the
    /// completed receipt instead of labelling a committed append as cancelled.
    pub(in crate::platform) fn commit_result<T>(
        &self,
        commit: impl FnOnce() -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut job = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *job.cancel.borrow() {
            return Err(ApiError::task_cancelled());
        }
        let result = commit()?;
        job.completion_committed = true;
        Ok(result)
    }

    pub(in crate::platform) fn result_budget() -> usize {
        MAX_RESULT_BYTES
    }

    pub(super) fn finish(&self, outcome: Result<Json<Value>, ApiError>) {
        // Size validation happens before locking a job, and never locks the registry.
        let outcome = outcome.and_then(|Json(result)| {
            if fits_retention_limit(&result) { Ok(Json(result)) } else {
                Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "deferred result exceeds the 2 MiB retention limit; use synchronous execution or smaller batches").with_code("task_job_result_too_large"))
            }
        });
        let mut job = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.finished = Some(Instant::now());
        if self.journal.is_some() {
            job.persistence_state = "pending";
        }
        job.detail = None;
        let outcome = if *job.cancel.borrow() {
            Err(ApiError::task_cancelled())
        } else {
            outcome
        };
        match outcome {
            Ok(Json(result)) => {
                job.status = JobStatus::Completed;
                job.reason = "completed";
                job.result = Some(result);
            }
            Err(error) => {
                let body = error.into_batch_result(self.id.clone());
                job.status = match body["code"].as_str() {
                    Some("task_cancelled" | "session_killed") => JobStatus::Cancelled,
                    Some("task_deadline_exceeded") => JobStatus::Expired,
                    _ => JobStatus::Failed,
                };
                job.reason = match job.status {
                    JobStatus::Cancelled => "cancelled",
                    JobStatus::Expired => "deadline_exceeded",
                    _ => "failed",
                };
                job.error = Some(if fits_retention_limit(&body) {
                    body
                } else {
                    json!({"code":"task_job_error_too_large", "error":"deferred error exceeds the 2 MiB retention limit"})
                });
            }
        }
    }
}
