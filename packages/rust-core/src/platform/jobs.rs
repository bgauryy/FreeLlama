//! Bounded deferred managed tasks with opt-in durable receipts. Admission and execution stay
//! owned by the ordinary task path; uncertain jobs are never replayed after a restart.
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

pub(super) const MAX_JOBS: usize = 64;
pub(super) const RESULT_TTL: Duration = Duration::from_secs(600);
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
    persistence_error: Option<Value>,
    persistence_state: &'static str,
}

impl Job {
    fn receipt(&self, include_result: bool) -> Value {
        let mut receipt = json!({
            "id": self.id, "status": self.status, "task": self.task, "priority": self.priority,
            "requested_model": self.requested_model, "selected_model": self.selected_model,
            "backend": self.backend, "reason": self.reason, "detail": self.detail,
            "created_at": self.created_at, "timeout_seconds": self.timeout_seconds,
            "deadline_at": self.created_at.saturating_add(self.timeout_seconds),
            "persistence_error": self.persistence_error,
            "persistence": {"state":self.persistence_state,"stage":if self.finished.is_some(){"terminal"}else{"acceptance"}},
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
    journal: Option<super::job_journal::JobJournal>,
}

impl JobRegistry {
    pub(super) fn configured(config: &super::PlatformConfig) -> anyhow::Result<Self> {
        Ok(Self {
            entries: BTreeMap::new(),
            journal: super::job_journal::JobJournal::configured(config)?,
        })
    }

    fn scope(&self) -> &'static str {
        if self.journal.is_some() {
            "database"
        } else {
            "process_memory"
        }
    }

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
    journal: Option<super::job_journal::JobJournal>,
}

impl JobProgress {
    async fn persist_terminal(&self) {
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

struct PreparedSubmission {
    id: String,
    /// Keyed hashes; present only when durable jobs are enabled.
    identity: Option<super::job_journal::Identity>,
    job: Arc<Mutex<Job>>,
    cancellation: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    timeout: Duration,
    deadline: tokio::time::Instant,
}

fn prepare(
    input: &mut TaskInput,
    journal: Option<&super::job_journal::JobJournal>,
) -> Result<PreparedSubmission, ApiError> {
    let durable = journal.is_some();
    let timeout = execution::task_deadline(input.timeout_seconds)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let key = input.idempotency_key.take();
    if key.is_some() && !durable {
        return Err(ApiError::bad_request(
            "idempotency_key requires durable deferred jobs to be enabled",
        ));
    }
    if key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > 128)
    {
        return Err(ApiError::bad_request(
            "idempotency_key must contain 1 to 128 UTF-8 bytes",
        ));
    }
    let input_bytes = serde_json::to_vec(&input).map_err(ApiError::bad_request)?;
    if input_bytes.len() > MAX_INPUT_BYTES {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "deferred input exceeds the 1 MiB retention limit; use synchronous execution or a smaller input")
            .with_code("task_job_input_too_large"));
    }
    let (cancel, cancellation) = watch::channel(false);
    let (done, done_receiver) = watch::channel(false);
    let id = Uuid::new_v4().to_string();
    // Field presence changes scoped defaults: omitted `task` and explicit `task:"completion"`
    // may have equal deserialized values but different execution semantics.
    let identity = journal
        .map(|journal| {
            journal.identity(
                &input.client,
                key.as_deref(),
                &id,
                &serde_json::to_vec(&(&input.route_fields, &*input))?,
            )
        })
        .transpose()
        .map_err(ApiError::bad_request)?;
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
        persistence_error: None,
        persistence_state: if durable { "confirmed" } else { "disabled" },
    };
    Ok(PreparedSubmission {
        id,
        identity,
        job: Arc::new(Mutex::new(job)),
        cancellation,
        done,
        timeout,
        deadline,
    })
}

fn deduplicated(
    state: &PlatformState,
    existing: &crate::store::jobs::JobRecord,
    input_hash: &str,
) -> Result<Json<Value>, ApiError> {
    if existing.input_hash != input_hash {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_key was already used with different input",
        )
        .with_code("task_job_idempotency_conflict"));
    }
    let original = match lookup(&state.jobs, &existing.id) {
        Ok(job) => job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .receipt(true),
        Err(_) => super::job_journal::recovered_receipt(existing)?,
    };
    Ok(Json(
        json!({"deferred":true,"deduplicated":true,"job":original,"retention":retention(true)}),
    ))
}

pub(super) fn validate_synchronous(input: &TaskInput) -> Result<(), ApiError> {
    if input.idempotency_key.is_some() {
        return Err(ApiError::bad_request(
            "idempotency_key is accepted only for deferred tasks",
        ));
    }
    Ok(())
}

async fn refuse_registration(progress: &JobProgress, error: ApiError) -> ApiError {
    progress.finish(Err(error));
    if let Some(error) = progress
        .job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .error
        .as_mut()
    {
        error["execution_outcome"] = json!("not_started");
        error["retry_safe"] = json!(true);
    }
    progress.persist_terminal().await;
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "deferred task registry is full; this submission did not execute",
    )
    .with_code("task_jobs_full")
    .with_retry_after(5)
}

pub(super) async fn submit(
    state: PlatformState,
    mut input: TaskInput,
) -> Result<Json<Value>, ApiError> {
    super::require_active_session(&state, input.route.session_id.as_deref()).await?;
    let journal = journal(&state);
    let PreparedSubmission {
        id,
        identity,
        job,
        cancellation,
        done,
        timeout,
        deadline,
    } = prepare(&mut input, journal.as_ref())?;
    let receipt = job
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .receipt(false);
    // Reuse an existing key before local capacity checks. A full local registry cannot turn a
    // retry of accepted work into a no-execution-safe refusal.
    if let (Some(journal), Some(identity)) = (&journal, identity) {
        let input_hash = identity.input_hash.clone();
        if let Some(existing) = journal
            .claim(
                id.clone(),
                identity,
                receipt.clone(),
                super::telemetry::now_seconds()
                    .saturating_add(timeout.as_secs())
                    .saturating_add(RESULT_TTL.as_secs()),
            )
            .await?
        {
            return deduplicated(&state, &existing, &input_hash);
        }
    }
    let durable = journal.is_some();
    let progress = JobProgress {
        job: job.clone(),
        id: id.clone(),
        cancellation,
        deadline,
        journal,
    };
    let registration_result = {
        let mut registry = state
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.make_room().map(|()| {
            registry.entries.insert(id.clone(), job);
        })
    };
    if let Err(error) = registration_result {
        return Err(refuse_registration(&progress, error).await);
    }
    let mut registration = PendingRegistration {
        registry: state.jobs.clone(),
        id,
        committed: false,
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
        progress.persist_terminal().await;
        let _ = done.send(true);
    });
    registration.committed = true;
    Ok(Json(
        json!({"deferred":true,"job":receipt,"retention":retention(durable)}),
    ))
}

struct PendingRegistration {
    registry: Arc<Mutex<JobRegistry>>,
    id: String,
    committed: bool,
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if !self.committed {
            self.registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entries
                .remove(&self.id);
        }
    }
}

fn retention(durable: bool) -> Value {
    json!({"scope":if durable { "database" } else { "process_memory" },"max_jobs":MAX_JOBS,
        "terminal_ttl_seconds":RESULT_TTL.as_secs(),"max_result_bytes":MAX_RESULT_BYTES,
        "max_durable_records":if durable { Some(crate::store::jobs::MAX_DURABLE_JOBS) } else {None},
        "max_durable_receipt_bytes":if durable {Some(crate::store::jobs::MAX_DURABLE_BYTES)} else {None},
        "durable_sync":if durable {Some("sqlite_wal_full")}else{None},
        "max_input_bytes":MAX_INPUT_BYTES,"restart":if durable { "receipts_recover_unfinished_outcomes_unknown_no_replay" } else { "jobs_are_not_resumed" }})
}

fn journal(state: &PlatformState) -> Option<super::job_journal::JobJournal> {
    state
        .jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .journal
        .clone()
}

pub(super) fn snapshot(jobs: &Mutex<JobRegistry>) -> Value {
    let (handles, scope) = {
        let mut registry = jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.prune();
        (
            registry.entries.values().cloned().collect::<Vec<_>>(),
            registry.scope(),
        )
    };
    json!({"jobs":handles.iter().map(|job| job.lock().unwrap_or_else(std::sync::PoisonError::into_inner).receipt(false)).collect::<Vec<_>>(),"scope":scope})
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

pub(super) async fn list(State(state): State<PlatformState>) -> Result<Json<Value>, ApiError> {
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

pub(super) async fn get(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
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
    if lookup(&state.jobs, &id).is_err() {
        // Recovery never claims to cancel a worker owned by a different process.
        return get(State(state), Path(id)).await;
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
pub(super) async fn remove(
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

    fn durable_state() -> (tempfile::TempDir, PlatformState) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("jobs.sqlite")).unwrap();
        let config = super::super::PlatformConfig::new(
            "127.0.0.1:11435",
            "http://127.0.0.1:11434",
            None,
            None,
        )
        .with_store(store)
        .with_durable_jobs(true);
        (dir, super::super::build(&config).unwrap().1)
    }

    fn keyed_input(key: &str) -> TaskInput {
        serde_json::from_value(
            json!({"task":"completion","prompt":"test","defer":true,"idempotency_key":key}),
        )
        .unwrap()
    }

    async fn claim_prepared(state: &PlatformState, prepared: &PreparedSubmission) {
        let receipt = prepared.job.lock().unwrap().receipt(false);
        assert!(
            journal(state)
                .unwrap()
                .claim(
                    prepared.id.clone(),
                    prepared.identity.clone().unwrap(),
                    receipt,
                    super::super::telemetry::now_seconds() + 600,
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    fn fill_active(state: &PlatformState) {
        let mut registry = state.jobs.lock().unwrap();
        while registry.entries.len() < MAX_JOBS {
            let current = job(None);
            registry
                .entries
                .insert(current.id.clone(), Arc::new(Mutex::new(current)));
        }
    }

    #[tokio::test]
    async fn durable_retries_deduplicate_at_local_capacity_and_include_terminal_results() {
        let (_dir, state) = durable_state();
        let input = keyed_input("same-work");
        let prepared = prepare(&mut input.clone(), journal(&state).as_ref()).unwrap();
        claim_prepared(&state, &prepared).await;
        state
            .jobs
            .lock()
            .unwrap()
            .entries
            .insert(prepared.id.clone(), prepared.job.clone());
        fill_active(&state);
        let receipt = submit(state.clone(), input.clone()).await.unwrap().0;
        assert_eq!(receipt["job"]["id"], prepared.id);
        assert_eq!(receipt["job"]["status"], "queued");
        assert_eq!(receipt["deduplicated"], true);
        {
            let mut original = prepared.job.lock().unwrap();
            original.status = JobStatus::Completed;
            original.finished = Some(Instant::now());
            original.result = Some(json!({"answer":42}));
        }
        let receipt = submit(state.clone(), input.clone()).await.unwrap().0;
        assert_eq!(receipt["job"]["result"]["answer"], 42);
        assert_eq!(state.jobs.lock().unwrap().entries.len(), MAX_JOBS);
        let omitted_task: TaskInput = serde_json::from_value(json!({
            "prompt":"test", "defer":true, "idempotency_key":"same-work",
        }))
        .unwrap();
        assert_eq!(omitted_task.route.task, input.route.task);
        let error = submit(state.clone(), omitted_task)
            .await
            .unwrap_err()
            .into_batch_result("test".into());
        assert_eq!(error["code"], "task_job_idempotency_conflict");
        let mut conflicting = input;
        conflicting.prompt = Some("different work".into());
        let error = submit(state, conflicting)
            .await
            .unwrap_err()
            .into_batch_result("test".into());
        assert_eq!(error["code"], "task_job_idempotency_conflict");
    }

    #[tokio::test]
    async fn durable_capacity_refusal_is_persisted_as_not_started_and_never_replayed() {
        let (_dir, state) = durable_state();
        fill_active(&state);
        let input = keyed_input("refused-work");
        let error = submit(state.clone(), input.clone())
            .await
            .unwrap_err()
            .into_batch_result("test".into());
        assert_eq!(error["code"], "task_jobs_full");
        let retained = journal(&state).unwrap().list().await.unwrap();
        assert_eq!(retained.len(), 1);
        let retry = submit(state.clone(), input).await.unwrap().0;
        assert_eq!(retry["deduplicated"], true);
        assert_eq!(retry["job"]["id"], retained[0]["id"]);
        assert_eq!(retry["job"]["status"], "failed");
        assert_eq!(retry["job"]["error"]["execution_outcome"], "not_started");
        assert_eq!(retry["job"]["error"]["retry_safe"], true);
        assert_eq!(retry["job"]["persistence"]["state"], "confirmed");
        assert_eq!(state.jobs.lock().unwrap().entries.len(), MAX_JOBS);
    }

    #[tokio::test]
    async fn cancellation_waits_for_pending_terminal_persistence_without_relabelling_completion() {
        let (_dir, state) = durable_state();
        let prepared =
            prepare(&mut keyed_input("completed-work"), journal(&state).as_ref()).unwrap();
        claim_prepared(&state, &prepared).await;
        state
            .jobs
            .lock()
            .unwrap()
            .entries
            .insert(prepared.id.clone(), prepared.job.clone());
        let progress = JobProgress {
            job: prepared.job.clone(),
            id: prepared.id.clone(),
            cancellation: prepared.cancellation,
            deadline: prepared.deadline,
            journal: journal(&state),
        };
        progress.finish(Ok(Json(json!({"answer":42}))));
        assert_eq!(
            prepared.job.lock().unwrap().receipt(true)["persistence"]["state"],
            "pending"
        );
        let cancel_state = state.clone();
        let cancel_id = prepared.id.clone();
        let mut waiting =
            tokio::spawn(async move { cancel(State(cancel_state), Path(cancel_id)).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        assert!(!*progress.cancellation.borrow());
        progress.persist_terminal().await;
        prepared.done.send(true).unwrap();
        let receipt = waiting.await.unwrap().unwrap().0;
        assert_eq!(receipt["job"]["status"], "completed");
        assert_eq!(receipt["job"]["persistence"]["state"], "confirmed");
        assert_eq!(
            journal(&state)
                .unwrap()
                .get(prepared.id)
                .await
                .unwrap()
                .unwrap()["result"]["answer"],
            42
        );
    }

    #[tokio::test]
    async fn terminal_commit_failure_is_visible_and_preserves_the_local_result() {
        let (_dir, state) = durable_state();
        let prepared =
            prepare(&mut keyed_input("unclaimed-work"), journal(&state).as_ref()).unwrap();
        let progress = JobProgress {
            job: prepared.job.clone(),
            id: prepared.id,
            cancellation: prepared.cancellation,
            deadline: prepared.deadline,
            journal: journal(&state),
        };
        // There is deliberately no durable claim, so the terminal transaction must fail.
        progress.finish(Ok(Json(json!({"answer":42}))));
        progress.persist_terminal().await;
        let receipt = prepared.job.lock().unwrap().receipt(true);
        assert_eq!(receipt["status"], "completed");
        assert_eq!(receipt["result"]["answer"], 42);
        assert_eq!(receipt["persistence"]["state"], "failed");
        assert_eq!(
            receipt["persistence_error"]["code"],
            "task_job_persistence_failed"
        );
    }

    #[tokio::test]
    async fn durable_receipts_recover_terminal_results_and_mark_interrupted_work_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("jobs.sqlite")).unwrap();
        let config = super::super::PlatformConfig::new(
            "127.0.0.1:11435",
            "http://127.0.0.1:11434",
            None,
            None,
        )
        .with_store(store)
        .with_durable_jobs(true);
        let (_, first) = super::super::build(&config).unwrap();
        let owner = journal(&first).unwrap();
        let id = "known-job".to_owned();
        owner
            .claim(
                id.clone(),
                super::super::job_journal::Identity {
                    request_key: "key".into(),
                    input_hash: "hash".into(),
                },
                json!({"id":id,"status":"queued"}),
                super::super::telemetry::now_seconds() + 600,
            )
            .await
            .unwrap();
        let (_, restarted) = super::super::build(&config).unwrap();
        let recovered = get(State(restarted.clone()), Path(id.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(
            recovered["job"]["error"]["code"],
            "task_job_outcome_unknown"
        );
        assert_eq!(recovered["job"]["error"]["retry_safe"], false);
        assert!(restarted.jobs.lock().unwrap().entries.is_empty());
        assert!(
            remove(State(restarted.clone()), Path(id.clone()))
                .await
                .is_err()
        );
        owner
            .finish(
                id.clone(),
                json!({"id":id,"status":"completed","result":{"answer":42}}),
            )
            .await
            .unwrap();
        let recovered = get(State(restarted.clone()), Path(id.clone()))
            .await
            .unwrap()
            .0;
        assert_eq!(recovered["job"]["result"]["answer"], 42);
        assert_eq!(
            list(State(restarted.clone())).await.unwrap().0["jobs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let _ = remove(State(restarted.clone()), Path(id.clone()))
            .await
            .unwrap();
        assert!(get(State(restarted), Path(id)).await.is_err());
    }

    #[test]
    fn dropped_submission_releases_its_local_registry_entry() {
        let mut registry = JobRegistry::default();
        let pending = job(None);
        let id = pending.id.clone();
        registry
            .entries
            .insert(id.clone(), Arc::new(Mutex::new(pending)));
        let registry = Arc::new(Mutex::new(registry));
        drop(PendingRegistration {
            registry: registry.clone(),
            id,
            committed: false,
        });
        assert!(registry.lock().unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn cancellation_receipt_survives_registry_eviction() {
        let config = super::super::PlatformConfig::new(
            "127.0.0.1:11435",
            "http://127.0.0.1:11434",
            None,
            None,
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
            persistence_error: None,
            persistence_state: "disabled",
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
            journal: None,
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
