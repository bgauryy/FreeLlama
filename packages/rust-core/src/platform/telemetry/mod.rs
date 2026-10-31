//! Monitoring: Prometheus counters, a usage ledger, and per-day usage totals.
//!
//! Counters are process-lifetime. The optional ledger is the shared database's `usage` table,
//! written by a bounded, asynchronous best-effort worker: successful appends are replayed at
//! startup; pending records are not durable yet.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex as StdMutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod ledger;

/// The longest window the usage view can show; `/usage?days=` is clamped to it and startup
/// replays only this window.
pub(crate) const REPORT_WINDOW_DAYS: u32 = 400;
mod prometheus;

use ledger::{civil_day, day_number, ledger_worker, replay, unix_day};
pub(super) use prometheus::Gauge;

pub(super) const LEDGER_QUEUE_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(128).expect("128 is nonzero");

enum LedgerCommand {
    Record(Box<TaskRecord>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// One finished managed task or raw generation (`task: "raw"`), as written to the ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct TaskRecord {
    /// Unix seconds when the task finished.
    pub(super) ts: u64,
    pub(super) model: String,
    pub(super) backend: String,
    pub(super) task: String,
    pub(super) priority: String,
    /// `ok`, or `error` with `status` holding the HTTP status returned to the caller.
    pub(super) outcome: String,
    pub(super) status: u16,
    #[serde(default)]
    pub(super) prompt_tokens: u64,
    #[serde(default)]
    pub(super) output_tokens: u64,
    /// Wall time of the whole managed call, queueing included.
    #[serde(default)]
    pub(super) duration_ms: u64,
    #[serde(default)]
    pub(super) queue_wait_ms: u64,
    #[serde(default)]
    pub(super) load_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) output_tokens_per_second: Option<f64>,
    /// Prompt tokens Ollama reused from the runner's prompt cache instead of evaluating.
    #[serde(default)]
    pub(super) cached_prompt_tokens: u64,
    /// The client key that made the request, when the server uses client keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) client: Option<String>,
}

impl TaskRecord {
    /// Tokens the backend had to process: output plus the prompt tokens it did not reuse.
    pub(super) fn processed_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_sub(self.cached_prompt_tokens)
            .saturating_add(self.output_tokens)
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub(super) struct UsageTotals {
    pub(super) tasks: u64,
    pub(super) errors: u64,
    pub(super) prompt_tokens: u64,
    pub(super) cached_prompt_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) busy_ms: u64,
    pub(super) queue_wait_ms: u64,
}

impl UsageTotals {
    fn add(&mut self, record: &TaskRecord) {
        self.tasks += 1;
        if record.outcome != "ok" {
            self.errors += 1;
        }
        self.prompt_tokens = self.prompt_tokens.saturating_add(record.prompt_tokens);
        self.cached_prompt_tokens = self
            .cached_prompt_tokens
            .saturating_add(record.cached_prompt_tokens);
        self.output_tokens = self.output_tokens.saturating_add(record.output_tokens);
        self.busy_ms = self.busy_ms.saturating_add(record.duration_ms);
        self.queue_wait_ms = self.queue_wait_ms.saturating_add(record.queue_wait_ms);
    }

    fn merge(&mut self, other: &Self) {
        self.tasks += other.tasks;
        self.errors += other.errors;
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.cached_prompt_tokens = self
            .cached_prompt_tokens
            .saturating_add(other.cached_prompt_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.busy_ms = self.busy_ms.saturating_add(other.busy_ms);
        self.queue_wait_ms = self.queue_wait_ms.saturating_add(other.queue_wait_ms);
    }
}

#[derive(Default)]
struct TaskCounters {
    count: u64,
    prompt_tokens: u64,
    output_tokens: u64,
    duration_ms: u64,
    queue_wait_ms: u64,
    load_ms: u64,
}

#[derive(Default)]
struct Inner {
    /// (model, backend, task, outcome)
    tasks: BTreeMap<(String, String, String, String), TaskCounters>,
    /// (day, model)
    usage: BTreeMap<(String, String), UsageTotals>,
    /// (day number, client) -> tokens processed for that client key.
    clients: BTreeMap<(u64, String), u64>,
    /// (kind, outcome) for raw passthrough requests.
    raw: BTreeMap<(String, String), u64>,
    /// backend -> models unloaded to make room.
    evictions: BTreeMap<String, u64>,
    /// backend -> times the upstream circuit opened.
    breaker_opens: BTreeMap<String, u64>,
    /// (backend, direction) adaptive-limit changes.
    limit_changes: BTreeMap<(String, String), u64>,
    ledger_error: Option<String>,
    ledger_records: u64,
    ledger_pending: u64,
    ledger_failed: u64,
    ledger_dropped: u64,
    /// backend -> exponentially weighted service time (wall time minus queue wait) of successful
    /// tasks.
    average_ms: BTreeMap<String, u64>,
}

/// Shared monitoring sink; cheap to clone.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<StdMutex<Inner>>,
    ledger: Option<crate::store::Store>,
    writer: Option<Arc<mpsc::SyncSender<LedgerCommand>>>,
    ledger_capacity: usize,
    started: Instant,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Telemetry")
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Telemetry {
    /// Create a sink, replaying daily totals from the `ledger` store when there is one.
    #[must_use]
    pub fn new(ledger: Option<crate::store::Store>) -> Self {
        Self::with_ledger_capacity(ledger, LEDGER_QUEUE_CAPACITY)
    }

    /// Configure the maximum queued writes; zero capacity is excluded by the type.
    #[must_use]
    pub fn with_ledger_capacity(
        ledger: Option<crate::store::Store>,
        capacity: NonZeroUsize,
    ) -> Self {
        let mut telemetry = Self {
            inner: Arc::new(StdMutex::new(Inner::default())),
            ledger,
            writer: None,
            ledger_capacity: capacity.get(),
            started: Instant::now(),
        };
        if let Some(store) = telemetry.ledger.clone() {
            match replay(&store) {
                Ok(days) => {
                    let mut inner = telemetry.lock();
                    for day in days {
                        inner.ledger_records += day.tasks;
                        inner.usage.insert(
                            (day.day, day.model),
                            UsageTotals {
                                tasks: day.tasks,
                                errors: day.errors,
                                prompt_tokens: day.prompt_tokens,
                                cached_prompt_tokens: day.cached_prompt_tokens,
                                output_tokens: day.output_tokens,
                                busy_ms: day.busy_ms,
                                queue_wait_ms: day.queue_wait_ms,
                            },
                        );
                    }
                }
                Err(error) => telemetry.lock().ledger_error = Some(error.to_string()),
            }
            let today = unix_day(now_seconds());
            match store.client_tokens_since(today * 86_400) {
                Ok(clients) => {
                    let mut inner = telemetry.lock();
                    for (client, tokens) in clients {
                        inner.clients.insert((today, client), tokens);
                    }
                }
                Err(error) => telemetry.lock().ledger_error = Some(error.to_string()),
            }
            let (sender, receiver) = mpsc::sync_channel(capacity.get());
            let inner = Arc::clone(&telemetry.inner);
            match std::thread::Builder::new()
                .name("freellama-ledger".into())
                .spawn(move || ledger_worker(&store, &inner, receiver))
            {
                Ok(_) => telemetry.writer = Some(Arc::new(sender)),
                Err(error) => telemetry.lock().ledger_error = Some(error.to_string()),
            }
        }
        telemetry
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // Synchronous: task completion never waits for disk or for room in the ledger queue.
    pub(super) fn record_task(&self, record: TaskRecord) {
        {
            let mut inner = self.lock();
            let counters = inner
                .tasks
                .entry((
                    record.model.clone(),
                    record.backend.clone(),
                    record.task.clone(),
                    record.outcome.clone(),
                ))
                .or_default();
            counters.count += 1;
            counters.prompt_tokens = counters.prompt_tokens.saturating_add(record.prompt_tokens);
            counters.output_tokens = counters.output_tokens.saturating_add(record.output_tokens);
            counters.duration_ms = counters.duration_ms.saturating_add(record.duration_ms);
            counters.queue_wait_ms = counters.queue_wait_ms.saturating_add(record.queue_wait_ms);
            counters.load_ms = counters.load_ms.saturating_add(record.load_ms);
            if record.outcome == "ok" && record.task != "raw" {
                // A raw response's time includes however its client read the stream.
                // Service time, not wall time: a task's own queue wait must not feed the wait
                // estimate given to the tasks queued behind it, or estimates snowball under load.
                let service_ms = record.duration_ms.saturating_sub(record.queue_wait_ms);
                let average = inner
                    .average_ms
                    .entry(record.backend.clone())
                    .or_insert(service_ms);
                *average = average.saturating_mul(4).saturating_add(service_ms) / 5;
            }
            add_usage(&mut inner, &record);
        }
        if self.ledger.is_none() {
            return;
        }
        // Never make task completion wait for disk or for room in the ledger queue.
        let mut inner = self.lock();
        inner.ledger_pending += 1;
        let queued = self
            .writer
            .as_ref()
            .map(|writer| writer.try_send(LedgerCommand::Record(Box::new(record))));
        if !matches!(queued, Some(Ok(()))) {
            inner.ledger_pending -= 1;
            inner.ledger_dropped += 1;
            inner.ledger_error = Some(
                match queued {
                    Some(Err(mpsc::TrySendError::Full(_))) => {
                        "Usage ledger queue is full; record dropped."
                    }
                    _ => "Usage ledger worker is unavailable; record dropped.",
                }
                .into(),
            );
        }
    }

    /// Wait at most `timeout` for previously queued records. A successful flush means the
    /// worker has attempted them, not that failed/dropped records were recovered or fsynced.
    /// Dropping the final sink closes the queue and lets its single worker drain and exit.
    pub async fn flush_ledger(&self, timeout: Duration) -> bool {
        if self.ledger.is_none() {
            return true;
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let flushed = tokio::time::timeout(timeout, async {
            let Some(writer) = &self.writer else {
                return false;
            };
            let mut command = LedgerCommand::Flush(sender);
            loop {
                match writer.try_send(command) {
                    Ok(()) => return receiver.await.is_ok(),
                    Err(mpsc::TrySendError::Disconnected(_)) => return false,
                    Err(mpsc::TrySendError::Full(returned)) => {
                        command = returned;
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            }
        })
        .await;
        if matches!(flushed, Ok(true)) {
            return true;
        }
        self.lock().ledger_error =
            Some("Usage ledger flush did not complete within its bounded wait.".into());
        false
    }

    /// Recent average wall time of successful tasks on `backend`, for `Retry-After` estimates.
    pub(super) fn average_task_ms(&self, backend: &str) -> Option<u64> {
        self.lock().average_ms.get(backend).copied()
    }

    pub(crate) fn record_raw(&self, kind: &str, outcome: &str) {
        *self
            .lock()
            .raw
            .entry((kind.to_owned(), outcome.to_owned()))
            .or_default() += 1;
    }

    pub(super) fn record_evictions(&self, backend: &str, count: usize) {
        if count > 0 {
            *self.lock().evictions.entry(backend.to_owned()).or_default() += count as u64;
        }
    }

    pub(super) fn record_breaker_open(&self, backend: &str) {
        *self
            .lock()
            .breaker_opens
            .entry(backend.to_owned())
            .or_default() += 1;
    }

    pub(super) fn record_limit_change(&self, backend: &str, direction: &str) {
        *self
            .lock()
            .limit_changes
            .entry((backend.to_owned(), direction.to_owned()))
            .or_default() += 1;
    }

    /// Daily totals for the last `days` days (including today), newest first, plus a per-model
    /// roll-up over the same window.
    pub(super) fn usage(&self, days: u32) -> Value {
        let today = unix_day(now_seconds());
        let first = today.saturating_sub(u64::from(days.max(1)) - 1);
        let inner = self.lock();
        let mut by_day: BTreeMap<&str, BTreeMap<&str, &UsageTotals>> = BTreeMap::new();
        let mut by_model: BTreeMap<&str, UsageTotals> = BTreeMap::new();
        let mut total = UsageTotals::default();
        for ((day, model), totals) in &inner.usage {
            if day_number(day).is_none_or(|number| number < first) {
                continue;
            }
            by_day.entry(day).or_default().insert(model, totals);
            by_model.entry(model).or_default().merge(totals);
            total.merge(totals);
        }
        json!({
            "window_days": days.max(1),
            "totals": total,
            "by_model": by_model,
            "by_day": by_day.into_iter().rev().map(|(day, models)| json!({
                "day": day,
                "models": models,
            })).collect::<Vec<_>>(),
            "ledger": {
                "path": self.ledger.as_ref().and_then(crate::store::Store::path),
                "records": inner.ledger_records,
                "last_error": inner.ledger_error,
                "pending": inner.ledger_pending,
                "failed": inner.ledger_failed,
                "dropped": inner.ledger_dropped,
                "capacity": self.ledger_capacity,
                "durability": "asynchronous_best_effort",
            },
        })
    }

    /// Tokens processed today (UTC) for `client`, as this process has seen them since it
    /// started plus what the shared database held then.
    pub(super) fn client_tokens_today(&self, client: &str) -> u64 {
        let today = unix_day(now_seconds());
        self.lock()
            .clients
            .get(&(today, client.to_owned()))
            .copied()
            .unwrap_or(0)
    }

    /// Today's processed tokens per client key.
    pub(super) fn clients_today(&self) -> BTreeMap<String, u64> {
        let today = unix_day(now_seconds());
        self.lock()
            .clients
            .iter()
            .filter(|((day, _), _)| *day == today)
            .map(|((_, client), tokens)| (client.clone(), *tokens))
            .collect()
    }

    pub(super) fn ledger_receipt(&self) -> Value {
        let inner = self.lock();
        json!({
            "enabled": self.ledger.is_some(),
            "path": self.ledger.as_ref().and_then(crate::store::Store::path),
            "records": inner.ledger_records,
            "last_error": inner.ledger_error,
            "pending": inner.ledger_pending,
            "failed": inner.ledger_failed,
            "dropped": inner.ledger_dropped,
            "capacity": self.ledger_capacity,
            "durability": "asynchronous_best_effort",
        })
    }
}

fn add_usage(inner: &mut Inner, record: &TaskRecord) {
    if let Some(client) = &record.client {
        let tokens = inner
            .clients
            .entry((unix_day(record.ts), client.clone()))
            .or_default();
        *tokens = tokens.saturating_add(record.processed_tokens());
    }
    inner
        .usage
        .entry((civil_day(unix_day(record.ts)), record.model.clone()))
        .or_default()
        .add(record);
}

pub(super) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
#[cfg(test)]
mod tests;
