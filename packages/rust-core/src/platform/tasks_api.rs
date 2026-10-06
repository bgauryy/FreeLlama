//! Task and batch request shapes and the batch runner.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::task::JoinSet;

use super::PlatformState;
use super::admission::{PRIORITY_WEIGHTS, priority_index};
use super::error::ApiError;
use super::{RouteInput, TaskPriority, execution, jobs, warming};

/// `remote = "Self"` makes the derives inherent functions; the trait impls below wrap them so
/// deserialization can also record which top-level fields the caller sent (`route_fields`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub(crate) struct TaskInput {
    #[serde(flatten)]
    pub(crate) route: RouteInput,
    pub(crate) scope_id: Option<String>,
    pub(crate) scope_revision: Option<u64>,
    #[serde(skip)]
    pub(crate) route_fields: BTreeSet<String>,
    #[serde(skip)]
    pub(crate) operation: warming::ManagedOperation,
    #[serde(default)]
    pub(crate) messages: Vec<Value>,
    pub(crate) prompt: Option<String>,
    /// Base64-encoded images (Ollama's own `images` format — no data URI prefix) attached to the
    /// single message built from `prompt`. For multi-turn `messages`, put `images` directly on
    /// the relevant message object instead; this field only applies to the `prompt` convenience
    /// path.
    pub(crate) images: Option<Vec<String>>,
    pub(crate) input: Option<Value>,
    /// Service class affects admission only; it never changes model selection or bypasses a
    /// backend's weighted capacity.
    #[serde(default)]
    pub(crate) priority: TaskPriority,
    pub(crate) tools: Option<Value>,
    /// Explicit caller residency override; omission uses the finite adaptive policy.
    pub(crate) keep_alive: Option<String>,
    /// Advanced Ollama controls which do not belong to routing. `num_ctx` stays owned by
    /// `context_tokens`, and backend placement owns `num_gpu`, so the route receipt always matches
    /// what is sent upstream.
    #[serde(default)]
    pub(crate) request_options: OllamaRequestOptions,
    /// Per-request admission wait in seconds, capped by the server's `max_queue_wait_seconds`.
    #[serde(default)]
    pub(crate) max_wait_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) defer: bool,
    /// Total time including discovery, queueing, loading and generation; capped by the server.
    pub(crate) timeout_seconds: Option<u64>,
    #[serde(skip)]
    pub(crate) job_progress: Option<jobs::JobProgress>,
}

impl Serialize for TaskInput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for TaskInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let fields = value
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default();
        let mut input = Self::deserialize(value).map_err(serde::de::Error::custom)?;
        input.route_fields = fields;
        Ok(input)
    }
}

/// Explicitly independent managed work. Dependencies are intentionally not accepted: a batch is
/// a bounded dispatcher, not a workflow engine, so an agent cannot accidentally run a dependent
/// step before the value it needs exists.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchTaskInput {
    pub(crate) id: String,
    independent: bool,
    pub(crate) task: TaskInput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskBatchInput {
    pub(crate) tasks: Vec<BatchTaskInput>,
    max_parallelism: Option<usize>,
}

const MAX_BATCH_TASKS: usize = 64;

const DEFAULT_BATCH_PARALLELISM: usize = 8;

fn next_batch_task(
    pending: &mut Vec<usize>,
    tasks: &[BatchTaskInput],
    credits: &mut [u8; 3],
) -> usize {
    for pass in 0..2 {
        for (class, credit) in credits.iter_mut().enumerate() {
            if *credit == 0 {
                continue;
            }
            if let Some(position) = pending
                .iter()
                .position(|index| priority_index(tasks[*index].task.priority) == class)
            {
                *credit -= 1;
                return pending.remove(position);
            }
        }
        if pass == 0 {
            *credits = PRIORITY_WEIGHTS;
        }
    }
    // `pending` is non-empty and every priority maps to a class, so this is unreachable unless a
    // future enum variant forgets its scheduler mapping.
    pending.remove(0)
}

pub(crate) async fn run_task_batch(
    State(state): State<PlatformState>,
    input: Result<Json<TaskBatchInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    if input.tasks.is_empty() || input.tasks.len() > MAX_BATCH_TASKS {
        return Err(ApiError::bad_request(format!(
            "tasks must contain 1 to {MAX_BATCH_TASKS} items"
        )));
    }
    let mut ids = BTreeSet::new();
    for item in &input.tasks {
        if item.id.trim().is_empty() || !ids.insert(item.id.clone()) {
            return Err(ApiError::bad_request(
                "every batch item needs a distinct, non-empty id",
            ));
        }
        if item.task.defer {
            return Err(ApiError::bad_request(
                "batch tasks cannot defer; submit individual jobs instead",
            ));
        }
        if !item.independent {
            return Err(ApiError::bad_request(format!(
                "batch task {} must declare independent=true; dependency scheduling is not supported",
                item.id
            )));
        }
    }
    let max_parallelism = input
        .max_parallelism
        .unwrap_or(DEFAULT_BATCH_PARALLELISM)
        .clamp(1, MAX_BATCH_TASKS)
        .min(input.tasks.len());
    let mut pending = (0..input.tasks.len()).collect::<Vec<_>>();
    let mut credits = PRIORITY_WEIGHTS;
    let mut results = std::iter::repeat_with(|| None)
        .take(input.tasks.len())
        .collect::<Vec<Option<Value>>>();
    let mut running = JoinSet::new();
    // A panicking worker fails only its own item; completed answers are kept.
    let mut in_flight = BTreeMap::new();

    while !pending.is_empty() || !running.is_empty() {
        while running.len() < max_parallelism && !pending.is_empty() {
            let index = next_batch_task(&mut pending, &input.tasks, &mut credits);
            let task = input.tasks[index].task.clone();
            let task_state = state.clone();
            let handle = running.spawn(Box::pin(execution::execute_task(
                State(task_state),
                Json(task),
            )));
            in_flight.insert(handle.id(), index);
        }
        if let Some(joined) = running.join_next_with_id().await {
            let (task_id, outcome) = match joined {
                Ok((task_id, result)) => (task_id, Ok(result)),
                Err(error) => (error.id(), Err(error)),
            };
            let Some(index) = in_flight.remove(&task_id) else {
                continue;
            };
            let id = input.tasks[index].id.clone();
            results[index] = Some(match outcome {
                Ok(Ok(Json(response))) => json!({ "id": id, "ok": true, "response": response }),
                Ok(Err(error)) => error.into_batch_result(id),
                Err(error) => ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("batch worker failed: {error}"),
                )
                .into_batch_result(id),
            });
        }
    }
    Ok(Json(json!({
        "independent_only": true,
        "max_parallelism": max_parallelism,
        "scheduler": {
            "scope": "batch_dispatch_and_global_managed_admission",
            "policy": "weighted_fair_round_robin",
            "weights": { "interactive": 3, "normal": 2, "background": 1 },
            "note": "priority affects start order and admission fairness, never route eligibility or capacity",
        },
        "results": results.into_iter().flatten().collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct OllamaRequestOptions {
    pub(crate) format: Option<Value>,
    pub(crate) think: Option<Value>,
    pub(crate) options: Option<Map<String, Value>>,
    pub(crate) logprobs: Option<bool>,
    pub(crate) top_logprobs: Option<u32>,
}
