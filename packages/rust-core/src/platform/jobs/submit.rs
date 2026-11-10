//! Job submission: input validation, durable idempotency claims, registration and spawn.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use crate::platform::{PlatformState, TaskInput, error::ApiError, execution};

use super::progress::JobProgress;
use super::registry::{PendingRegistration, RegistryFull, journal, lookup, retention};
use super::{Job, JobStatus, MAX_INPUT_BYTES, RESULT_TTL};

pub(super) struct PreparedSubmission {
    pub(super) id: String,
    /// Keyed hashes; present only when durable jobs are enabled.
    pub(super) identity: Option<crate::platform::job_journal::Identity>,
    pub(super) job: Arc<Mutex<Job>>,
    pub(super) cancellation: watch::Receiver<bool>,
    pub(super) done: watch::Sender<bool>,
    pub(super) timeout: Duration,
    pub(super) deadline: tokio::time::Instant,
}

pub(super) fn prepare(
    input: &mut TaskInput,
    journal: Option<&crate::platform::job_journal::JobJournal>,
) -> Result<PreparedSubmission, ApiError> {
    let durable = journal.is_some();
    execution::nearest::validate(input)?;
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
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "job input is {} bytes, over the {} MiB limit; split it into smaller jobs",
                input_bytes.len(),
                MAX_INPUT_BYTES / (1024 * 1024)
            ),
        )
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
        created_at: crate::platform::telemetry::now_seconds(),
        timeout_seconds: timeout.as_secs(),
        finished: None,
        result: None,
        error: None,
        cancel,
        done: done_receiver,
        completion_committed: false,
        persistence_error: None,
        persistence_state: if durable { "confirmed" } else { "disabled" },
        input_bytes: input_bytes.len(),
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
        Err(_) => crate::platform::job_journal::recovered_receipt(existing)?,
    };
    Ok(Json(
        json!({"deferred":true,"deduplicated":true,"job":original,"retention":retention(true)}),
    ))
}

pub(in crate::platform) fn validate_synchronous(input: &TaskInput) -> Result<(), ApiError> {
    if input.idempotency_key.is_some() {
        return Err(ApiError::bad_request(
            "idempotency_key is accepted only for deferred tasks",
        ));
    }
    Ok(())
}

async fn refuse_registration(progress: &JobProgress, refusal: RegistryFull) -> ApiError {
    progress.finish(Err(refusal.error("")));
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
    refusal.error("; this submission did not execute")
}

pub(in crate::platform) async fn submit(
    state: PlatformState,
    mut input: TaskInput,
) -> Result<Json<Value>, ApiError> {
    crate::platform::require_active_session(&state, input.route.session_id.as_deref()).await?;
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
                crate::platform::telemetry::now_seconds()
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
        let input_bytes = job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input_bytes;
        registry.make_room(input_bytes).map(|()| {
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
