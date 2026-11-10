//! Registry, submission, progress and handler tests that share job and durable-state fixtures.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use super::handlers::{cancel, get, list, remove};
use super::progress::JobProgress;
use super::registry::{
    JobRegistry, PendingRegistration, RegistryFull, fits_retention_limit, journal,
};
use super::submit::{PreparedSubmission, prepare, submit};
use super::*;
use crate::platform::{PlatformState, TaskInput};

fn durable_state() -> (tempfile::TempDir, PlatformState) {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("jobs.sqlite")).unwrap();
    let config =
        super::super::PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
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
    let prepared = prepare(&mut keyed_input("completed-work"), journal(&state).as_ref()).unwrap();
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
    let prepared = prepare(&mut keyed_input("unclaimed-work"), journal(&state).as_ref()).unwrap();
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
    let config =
        super::super::PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
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
    let config =
        super::super::PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None);
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
    let receipt = cancel(State(state), Path(id))
        .await
        .expect("accepted cancellation must keep its receipt even when retention evicts the job");
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
        input_bytes: 0,
    }
}

#[test]
fn pending_input_is_budgeted_across_unfinished_jobs_only() {
    let mut registry = JobRegistry::default();
    let mut held = job(None);
    held.input_bytes = MAX_PENDING_INPUT_BYTES - 10;
    let held_id = held.id.clone();
    registry
        .entries
        .insert(held_id.clone(), Arc::new(Mutex::new(held)));
    registry.make_room(10).unwrap();
    let refusal = registry.make_room(11).unwrap_err();
    assert_eq!(refusal, RegistryFull::PendingInput);
    let error = refusal.error("").into_batch_result("new".into());
    assert_eq!(error["code"], "task_jobs_input_budget");
    assert_eq!(error["retry_after_seconds"], 5);
    // A finished job has dropped its input, so it no longer counts.
    registry.entries[&held_id].lock().unwrap().finished = Some(Instant::now());
    registry.make_room(MAX_INPUT_BYTES).unwrap();
}

#[test]
fn a_job_input_over_the_limit_is_refused_with_its_size() {
    let mut input: TaskInput = serde_json::from_value(json!({
        "task":"embedding", "defer":true, "input":["x".repeat(MAX_INPUT_BYTES)],
    }))
    .unwrap();
    let Err(error) = prepare(&mut input, None) else {
        panic!("an oversized input must be refused");
    };
    let error = error.into_batch_result("big".into());
    assert_eq!(error["code"], "task_job_input_too_large");
    assert!(error["error"].as_str().unwrap().contains("16 MiB"));
    let mut fits: TaskInput = serde_json::from_value(json!({
        "task":"embedding", "defer":true, "input":["x".repeat(2 * 1024 * 1024)],
    }))
    .unwrap();
    assert!(prepare(&mut fits, None).is_ok());
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
        .make_room(0)
        .unwrap_err()
        .error("")
        .into_batch_result("new".into());
    assert_eq!(error["code"], "task_jobs_full");
    let evicted = registry.entries.keys().next().unwrap().clone();
    let mut oldest = registry.entries[&evicted].lock().unwrap();
    oldest.finished = Some(Instant::now());
    oldest.status = JobStatus::Completed;
    drop(oldest);
    registry.make_room(0).unwrap();
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
