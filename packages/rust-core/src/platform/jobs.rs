//! Bounded, process-local handles for deferred managed tasks. Admission and execution stay owned
//! by the ordinary task path; this registry never grants its own inference permits.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use super::{PlatformState, TaskInput, TaskKind, TaskPriority, error::ApiError, execution};

const MAX_JOBS: usize = 64;
const RESULT_TTL: Duration = Duration::from_secs(600);
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum JobStatus {
    Queued,
    WaitingForAdmission,
    WaitingForResources,
    WaitingForRunner,
    Loading,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl JobStatus {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

#[derive(Debug)]
struct Job {
    id: String,
    status: JobStatus,
    task: TaskKind,
    priority: TaskPriority,
    requested_model: Option<String>,
    selected_model: Option<String>,
    backend: Option<String>,
    reason: &'static str,
    detail: Option<Value>,
    created_at: u64,
    timeout_seconds: u64,
    finished: Option<Instant>,
    result: Option<Value>,
    error: Option<Value>,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    completion_committed: bool,
}

impl Job {
    fn receipt(&self, include_result: bool) -> Value {
        let mut receipt = json!({
            "id": self.id, "status": self.status, "task": self.task, "priority": self.priority,
            "requested_model": self.requested_model, "selected_model": self.selected_model,
            "backend": self.backend, "reason": self.reason, "detail": self.detail,
            "created_at": self.created_at, "timeout_seconds": self.timeout_seconds,
            "deadline_at": self.created_at.saturating_add(self.timeout_seconds),
        });
        if include_result {
            receipt["result"] = json!(self.result);
            receipt["error"] = json!(self.error);
        }
        receipt
    }
}

#[derive(Debug, Default)]
pub(super) struct JobRegistry {
    entries: BTreeMap<String, Arc<Mutex<Job>>>,
}

impl JobRegistry {
    fn prune(&mut self) {
        self.entries.retain(|_, job| {
            job.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finished
                .is_none_or(|at| at.elapsed() < RESULT_TTL)
        });
    }

    fn make_room(&mut self) -> Result<(), ApiError> {
        self.prune();
        if self.entries.len() >= MAX_JOBS {
            let oldest = self
                .entries
                .values()
                .filter_map(|job| {
                    let job = job
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    job.finished.map(|at| (at, job.id.clone()))
                })
                .min_by_key(|(at, _)| *at)
                .map(|(_, id)| id);
            if let Some(id) = oldest {
                self.entries.remove(&id);
            }
        }
        if self.entries.len() >= MAX_JOBS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "deferred task registry is full",
            )
            .with_code("task_jobs_full")
            .with_retry_after(5));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(super) struct JobProgress {
    job: Arc<Mutex<Job>>,
    id: String,
    cancellation: watch::Receiver<bool>,
    deadline: tokio::time::Instant,
}

impl JobProgress {
    pub(super) fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancellation.clone()
    }

    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    pub(super) fn update(&self, status: JobStatus, reason: &'static str, detail: Option<Value>) {
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

    pub(super) fn route(&self, model: &str, backend: &str) {
        let mut job = self
            .job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.selected_model = Some(model.into());
        job.backend = Some(backend.into());
    }

    /// Linearize history commit against cancellation. After commit, cancellation waits for the
    /// completed receipt instead of labelling a committed append as cancelled.
    pub(super) fn commit_result<T>(
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

    pub(super) fn result_budget() -> usize {
        MAX_RESULT_BYTES
    }

    fn finish(&self, outcome: Result<Json<Value>, ApiError>) {
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

pub(super) async fn submit(
    state: PlatformState,
    mut input: TaskInput,
) -> Result<Json<Value>, ApiError> {
    super::require_active_session(&state, input.route.session_id.as_deref()).await?;
    let timeout = execution::task_deadline(input.timeout_seconds)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let input_bytes = serde_json::to_vec(&input)
        .map_err(ApiError::bad_request)?
        .len();
    if input_bytes > MAX_INPUT_BYTES {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "deferred input exceeds the 1 MiB retention limit; use synchronous execution or a smaller input")
            .with_code("task_job_input_too_large"));
    }
    let (cancel, cancellation) = watch::channel(false);
    let (done, done_receiver) = watch::channel(false);
    let id = Uuid::new_v4().to_string();
    let job = Job {
        id: id.clone(),
        status: JobStatus::Queued,
        task: input.route.task,
        priority: input.priority,
        requested_model: input.route.model.clone(),
        selected_model: None,
        backend: None,
        reason: "discovering_model",
        detail: None,
        created_at: super::telemetry::now_seconds(),
        timeout_seconds: timeout.as_secs(),
        finished: None,
        result: None,
        error: None,
        cancel,
        done: done_receiver,
        completion_committed: false,
    };
    let receipt = job.receipt(false);
    let job = Arc::new(Mutex::new(job));
    {
        let mut registry = state
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.make_room()?;
        registry.entries.insert(id.clone(), job.clone());
    }
    let progress = JobProgress {
        job,
        id,
        cancellation,
        deadline,
    };
    input.defer = false;
    input.timeout_seconds = Some(timeout.as_secs());
    input.job_progress = Some(progress.clone());
    // Supervising the worker turns a panic into a terminal receipt instead of an immortal job.
    tokio::spawn(async move {
        let outcome = tokio::spawn(execution::execute_task(State(state), Json(input)))
            .await
            .unwrap_or_else(|error| {
                Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error)
                    .with_code("task_job_worker_failed"))
            });
        progress.finish(outcome);
        let _ = done.send(true);
    });
    Ok(Json(json!({"deferred":true,"job":receipt,"retention":{
        "scope":"process_memory", "max_jobs":MAX_JOBS,"terminal_ttl_seconds":RESULT_TTL.as_secs(),
        "max_result_bytes":MAX_RESULT_BYTES,"max_input_bytes":MAX_INPUT_BYTES,"restart":"jobs_are_not_resumed",
    }})))
}

pub(super) fn snapshot(jobs: &Mutex<JobRegistry>) -> Value {
    let handles = {
        let mut registry = jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.prune();
        registry.entries.values().cloned().collect::<Vec<_>>()
    };
    json!({"jobs":handles.iter().map(|job| job.lock().unwrap_or_else(std::sync::PoisonError::into_inner).receipt(false)).collect::<Vec<_>>(),"scope":"process_memory"})
}

fn lookup(jobs: &Mutex<JobRegistry>, id: &str) -> Result<Arc<Mutex<Job>>, ApiError> {
    let mut registry = jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.prune();
    registry
        .entries
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "job does not exist or has expired"))
}

pub(super) async fn list(State(state): State<PlatformState>) -> Json<Value> {
    Json(snapshot(&state.jobs))
}

pub(super) async fn get(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = lookup(&state.jobs, &id)?;
    let receipt = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt(true);
    Ok(Json(json!({"job":receipt})))
}

pub(super) async fn cancel(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
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
        if current.status.terminal() {
            drop(current);
            return Ok(job);
        }
        if !current.completion_committed {
            current.status = JobStatus::Cancelling;
            current.reason = "cancellation_requested";
            let _ = current.cancel.send(true);
        }
        current.done.clone()
    };
    // A receipt saying cancelled is returned only after the worker has dropped its permits.
    done.wait_for(|finished| *finished)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "job worker unavailable"))?;
    Ok(job)
}

/// Stop only this job, wait for local permits, then discard its retained record and payload.
pub(super) async fn remove(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = cancel_job(&state, &id).await?;
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
        json!({"id":id,"removed":true,"status":status,"scope":"process_memory"}),
    ))
}

/// A counting writer bounds serialization without allocating a second result-sized buffer.
fn fits_retention_limit(value: &Value) -> bool {
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("retention limit"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Limit(MAX_RESULT_BYTES), value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_receipt_survives_registry_eviction() {
        let config = super::super::PlatformConfig::new(
            "127.0.0.1:11435",
            "http://127.0.0.1:11434",
            None,
            None,
            "helper:latest",
        );
        let (_, state) = super::super::build(&config).unwrap();
        let mut current = job(None);
        let id = current.id.clone();
        let mut cancellation = current.cancel.subscribe();
        let (done, receiver) = watch::channel(false);
        current.done = receiver;
        state
            .jobs
            .lock()
            .unwrap()
            .entries
            .insert(id.clone(), Arc::new(Mutex::new(current)));
        let registry = state.jobs.clone();
        let evicted_id = id.clone();
        tokio::spawn(async move {
            cancellation.wait_for(|cancelled| *cancelled).await.unwrap();
            let mut registry = registry.lock().unwrap();
            let handle = registry.entries[&evicted_id].clone();
            let mut job = handle.lock().unwrap();
            job.status = JobStatus::Cancelled;
            job.reason = "cancelled";
            job.finished = Some(Instant::now());
            registry.entries.remove(&evicted_id);
            done.send(true).unwrap();
        });
        let receipt = cancel(State(state), Path(id)).await.expect(
            "accepted cancellation must keep its receipt even when retention evicts the job",
        );
        assert_eq!(receipt.0["job"]["status"], "cancelled");
    }

    fn job(finished: Option<Instant>) -> Job {
        let (cancel, _) = watch::channel(false);
        let (_, done) = watch::channel(false);
        Job {
            id: Uuid::new_v4().to_string(),
            status: if finished.is_some() {
                JobStatus::Completed
            } else {
                JobStatus::Queued
            },
            task: TaskKind::Completion,
            priority: TaskPriority::Normal,
            requested_model: None,
            selected_model: None,
            backend: None,
            reason: "test",
            detail: None,
            created_at: 0,
            timeout_seconds: 10,
            finished,
            result: None,
            error: None,
            cancel,
            done,
            completion_committed: false,
        }
    }

    #[test]
    fn registry_bounds_active_work_and_evicts_only_finished_jobs() {
        let mut registry = JobRegistry::default();
        for _ in 0..MAX_JOBS {
            let job = job(None);
            registry
                .entries
                .insert(job.id.clone(), Arc::new(Mutex::new(job)));
        }
        let error = registry
            .make_room()
            .unwrap_err()
            .into_batch_result("new".into());
        assert_eq!(error["code"], "task_jobs_full");
        let evicted = registry.entries.keys().next().unwrap().clone();
        let mut oldest = registry.entries[&evicted].lock().unwrap();
        oldest.finished = Some(Instant::now());
        oldest.status = JobStatus::Completed;
        drop(oldest);
        registry.make_room().unwrap();
        assert_eq!(registry.entries.len(), MAX_JOBS - 1);
        assert!(!registry.entries.contains_key(&evicted));
        assert!(
            registry
                .entries
                .values()
                .all(|job| !job.lock().unwrap().status.terminal())
        );
    }

    #[test]
    fn retention_prunes_expired_results_and_bounds_payloads() {
        let mut registry = JobRegistry::default();
        let expired = job(Some(Instant::now().checked_sub(RESULT_TTL).unwrap()));
        registry
            .entries
            .insert(expired.id.clone(), Arc::new(Mutex::new(expired)));
        registry.prune();
        assert!(registry.entries.is_empty());
        let current = job(None);
        let id = current.id.clone();
        let cancellation = current.cancel.subscribe();
        registry
            .entries
            .insert(id.clone(), Arc::new(Mutex::new(current)));
        let registry = Arc::new(Mutex::new(registry));
        let progress = JobProgress {
            job: registry.lock().unwrap().entries[&id].clone(),
            id: id.clone(),
            cancellation,
            deadline: tokio::time::Instant::now() + Duration::from_secs(10),
        };
        progress.finish(Ok(Json(json!({"response":"x".repeat(MAX_RESULT_BYTES)}))));
        let registry = registry.lock().unwrap();
        let receipt = registry.entries[&id].lock().unwrap().receipt(true);
        assert_eq!(receipt["status"], "failed");
        assert_eq!(receipt["error"]["code"], "task_job_result_too_large");
        assert!(receipt["result"].is_null());
    }

    #[test]
    fn retention_counts_encoded_json_bytes_and_accepts_the_exact_boundary() {
        assert!(fits_retention_limit(&json!(
            "x".repeat(MAX_RESULT_BYTES - 2)
        )));
        assert!(!fits_retention_limit(&json!(
            "x".repeat(MAX_RESULT_BYTES - 1)
        )));
        assert!(!fits_retention_limit(&json!(
            "\n".repeat(MAX_RESULT_BYTES / 2)
        )));
        assert!(!fits_retention_limit(&json!(
            "é".repeat(MAX_RESULT_BYTES / 2)
        )));
    }
}
