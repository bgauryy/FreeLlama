//! Progress of a running task batch, readable while the batch call is still open, so a caller can
//! report "3 of 8 done, 2 running" instead of a silent wait. Opt-in per batch (`progress_id`),
//! process-local and bounded; a finished batch's record is kept briefly for a last read. Progress
//! is a view only: the batch response stays the source of every result.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use super::{PlatformState, error::ApiError};

/// Batches tracked at once; a batch beyond it runs without progress rather than being refused.
const MAX_TRACKED: usize = 32;
/// How long a finished batch's record stays readable.
const KEEP_FINISHED: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct BatchProgress {
    batches: Mutex<BTreeMap<String, Entry>>,
}

struct Entry {
    total: usize,
    succeeded: usize,
    failed: usize,
    running: BTreeSet<String>,
    started: Instant,
    finished: Option<Instant>,
}

impl BatchProgress {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start tracking `id`; false (no tracking) when the id is in use by a running batch or the
    /// registry is full of running batches.
    pub(super) fn begin(&self, id: &str, total: usize) -> bool {
        let mut batches = self.lock();
        batches.retain(|_, entry| {
            entry
                .finished
                .is_none_or(|finished| finished.elapsed() < KEEP_FINISHED)
        });
        if batches
            .get(id)
            .is_some_and(|entry| entry.finished.is_none())
        {
            return false;
        }
        if batches.len() >= MAX_TRACKED {
            let oldest_finished = batches
                .iter()
                .filter_map(|(key, entry)| entry.finished.map(|at| (at, key.clone())))
                .min();
            let Some((_, key)) = oldest_finished else {
                return false;
            };
            batches.remove(&key);
        }
        batches.insert(
            id.to_owned(),
            Entry {
                total,
                succeeded: 0,
                failed: 0,
                running: BTreeSet::new(),
                started: Instant::now(),
                finished: None,
            },
        );
        true
    }

    pub(super) fn started(&self, id: &str, item: &str) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.running.insert(item.to_owned());
        }
    }

    pub(super) fn finished(&self, id: &str, item: &str, ok: bool) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.running.remove(item);
            if ok {
                entry.succeeded += 1;
            } else {
                entry.failed += 1;
            }
        }
    }

    pub(super) fn end(&self, id: &str) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.finished = Some(Instant::now());
            entry.running.clear();
        }
    }

    fn view(&self, id: &str) -> Option<Value> {
        let batches = self.lock();
        let entry = batches.get(id)?;
        let done = entry.succeeded + entry.failed;
        Some(json!({
            "progress_id": id,
            "total": entry.total,
            "done": done,
            "succeeded": entry.succeeded,
            "failed": entry.failed,
            "running": entry.running,
            "queued": entry.total.saturating_sub(done + entry.running.len()),
            "finished": entry.finished.is_some(),
            "elapsed_ms": u64::try_from(entry.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
    }
}

/// A caller-chosen progress id: 1 to 64 ASCII letters, digits, `-` or `_`.
pub(super) fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) async fn get(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.batch_progress.view(&id).map(Json).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "no batch with this progress_id is running or recently finished",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_running_and_finished_items() {
        let progress = BatchProgress::default();
        assert!(progress.begin("b1", 3));
        assert!(!progress.begin("b1", 3), "a running id is not reused");
        progress.started("b1", "a");
        progress.started("b1", "b");
        progress.finished("b1", "a", true);
        let view = progress.view("b1").unwrap();
        assert_eq!(view["done"], 1);
        assert_eq!(view["running"], json!(["b"]));
        assert_eq!(view["queued"], 1);
        progress.finished("b1", "b", false);
        progress.end("b1");
        let view = progress.view("b1").unwrap();
        assert_eq!(
            (view["succeeded"].clone(), view["failed"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(view["finished"], true);
        assert!(progress.begin("b1", 2), "a finished id can be reused");
    }

    #[test]
    fn ids_are_short_and_plain() {
        assert!(valid_id("batch-01_a"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"x".repeat(65)));
    }
}
