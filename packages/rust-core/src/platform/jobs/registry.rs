//! The bounded in-process job registry: retention, eviction, input budget and lookups.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::platform::{PlatformState, error::ApiError};

use super::{
    Job, MAX_INPUT_BYTES, MAX_JOBS, MAX_PENDING_INPUT_BYTES, MAX_RESULT_BYTES, RESULT_TTL,
};

#[derive(Debug, Default)]
pub(in crate::platform) struct JobRegistry {
    pub(super) entries: BTreeMap<String, Arc<Mutex<Job>>>,
    pub(super) journal: Option<crate::platform::job_journal::JobJournal>,
}

impl JobRegistry {
    pub(in crate::platform) fn configured(
        config: &crate::platform::PlatformConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            entries: BTreeMap::new(),
            journal: crate::platform::job_journal::JobJournal::configured(config)?,
        })
    }

    fn scope(&self) -> &'static str {
        if self.journal.is_some() {
            "database"
        } else {
            "process_memory"
        }
    }

    pub(super) fn prune(&mut self) {
        self.entries.retain(|_, job| {
            job.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finished
                .is_none_or(|at| at.elapsed() < RESULT_TTL)
        });
    }

    pub(super) fn make_room(&mut self, input_bytes: usize) -> Result<(), RegistryFull> {
        self.prune();
        let pending: usize = self
            .entries
            .values()
            .map(|job| {
                let job = job
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if job.finished.is_none() {
                    job.input_bytes
                } else {
                    0
                }
            })
            .sum();
        if pending.saturating_add(input_bytes) > MAX_PENDING_INPUT_BYTES {
            return Err(RegistryFull::PendingInput);
        }
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
            return Err(RegistryFull::Jobs);
        }
        Ok(())
    }
}

/// Why the registry refused a job; both refusals happen before any work starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegistryFull {
    Jobs,
    PendingInput,
}

impl RegistryFull {
    pub(super) fn error(self, suffix: &str) -> ApiError {
        let (message, code) = match self {
            Self::Jobs => (
                "deferred task registry is full".to_owned(),
                "task_jobs_full",
            ),
            Self::PendingInput => (
                format!(
                    "queued and running jobs already hold the {} MiB input budget",
                    MAX_PENDING_INPUT_BYTES / (1024 * 1024)
                ),
                "task_jobs_input_budget",
            ),
        };
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, format!("{message}{suffix}"))
            .with_code(code)
            .with_retry_after(5)
    }
}

pub(super) struct PendingRegistration {
    pub(super) registry: Arc<Mutex<JobRegistry>>,
    pub(super) id: String,
    pub(super) committed: bool,
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

pub(super) fn retention(durable: bool) -> Value {
    json!({"scope":if durable { "database" } else { "process_memory" },"max_jobs":MAX_JOBS,
        "terminal_ttl_seconds":RESULT_TTL.as_secs(),"max_result_bytes":MAX_RESULT_BYTES,
        "max_durable_records":if durable { Some(crate::store::jobs::MAX_DURABLE_JOBS) } else {None},
        "max_durable_receipt_bytes":if durable {Some(crate::store::jobs::MAX_DURABLE_BYTES)} else {None},
        "durable_sync":if durable {Some("sqlite_wal_full")}else{None},
        "max_input_bytes":MAX_INPUT_BYTES,"max_pending_input_bytes":MAX_PENDING_INPUT_BYTES,"restart":if durable { "receipts_recover_unfinished_outcomes_unknown_no_replay" } else { "jobs_are_not_resumed" }})
}

pub(super) fn journal(state: &PlatformState) -> Option<crate::platform::job_journal::JobJournal> {
    state
        .jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .journal
        .clone()
}

pub(in crate::platform) fn snapshot(jobs: &Mutex<JobRegistry>) -> Value {
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

pub(super) fn lookup(jobs: &Mutex<JobRegistry>, id: &str) -> Result<Arc<Mutex<Job>>, ApiError> {
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

/// A counting writer bounds serialization without allocating a second result-sized buffer.
pub(super) fn fits_retention_limit(value: &Value) -> bool {
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
