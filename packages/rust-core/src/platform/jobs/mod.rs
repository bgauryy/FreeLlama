//! Bounded deferred managed tasks with opt-in durable receipts. Admission and execution stay
//! owned by the ordinary task path; uncertain jobs are never replayed after a restart.
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{TaskKind, TaskPriority};

mod handlers;
mod progress;
mod registry;
mod submit;
#[cfg(test)]
mod tests;

pub(super) use handlers::{cancel, get, list, remove};
pub(super) use progress::JobProgress;
pub(super) use registry::{JobRegistry, snapshot};
pub(super) use submit::{submit, validate_synchronous};

pub(super) const MAX_JOBS: usize = 64;
pub(super) const RESULT_TTL: Duration = Duration::from_secs(600);
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
/// One job's encoded input. Every managed MCP task runs as a job, so this bounds an embedding
/// over a whole file set or a screenshot, not only deferred work; 1 MiB refused both.
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// Input held by queued and running jobs together; a terminal job has dropped its input. This,
/// not the per-job limit, bounds the memory that 64 accepted jobs can pin.
const MAX_PENDING_INPUT_BYTES: usize = 64 * 1024 * 1024;

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
    /// Encoded input size, counted against [`MAX_PENDING_INPUT_BYTES`] until the job finishes.
    input_bytes: usize,
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
