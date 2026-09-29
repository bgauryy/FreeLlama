//! Monitoring: Prometheus counters, a usage ledger, and per-day usage totals.
//!
//! Counters are process-lifetime (Prometheus convention: a restart resets them and `rate()`
//! handles it). The usage ledger is the durable side: one JSON line per finished managed task,
//! appended to an optional file and replayed at startup so daily totals survive restarts.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{BufRead, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Rotate the ledger to `<file>.1` past this size; the replay reads at most this much.
const LEDGER_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Daily totals kept in memory.
const MAX_USAGE_DAYS: usize = 400;

/// One finished managed task, as written to the ledger.
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
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub(super) struct UsageTotals {
    pub(super) tasks: u64,
    pub(super) errors: u64,
    pub(super) prompt_tokens: u64,
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
        self.output_tokens = self.output_tokens.saturating_add(record.output_tokens);
        self.busy_ms = self.busy_ms.saturating_add(record.duration_ms);
        self.queue_wait_ms = self.queue_wait_ms.saturating_add(record.queue_wait_ms);
    }

    fn merge(&mut self, other: &Self) {
        self.tasks += other.tasks;
        self.errors += other.errors;
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
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
    /// backend -> exponentially weighted wall time of successful tasks.
    average_ms: BTreeMap<String, u64>,
}

/// Shared monitoring sink; cheap to clone.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<StdMutex<Inner>>,
    ledger: Option<Arc<PathBuf>>,
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
    /// Create a sink, replaying daily totals from `ledger` when it exists.
    #[must_use]
    pub fn new(ledger: Option<PathBuf>) -> Self {
        let telemetry = Self {
            inner: Arc::new(StdMutex::new(Inner::default())),
            ledger: ledger.map(Arc::new),
            started: Instant::now(),
        };
        if let Some(path) = telemetry.ledger.as_deref() {
            match replay(path) {
                Ok(records) => {
                    let mut inner = telemetry.lock();
                    for record in &records {
                        add_usage(&mut inner, record);
                    }
                    inner.ledger_records = records.len() as u64;
                }
                Err(error) => telemetry.lock().ledger_error = Some(error.to_string()),
            }
        }
        telemetry
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("telemetry state poisoned")
    }

    pub(super) async fn record_task(&self, record: TaskRecord) {
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
            if record.outcome == "ok" {
                let average = inner
                    .average_ms
                    .entry(record.backend.clone())
                    .or_insert(record.duration_ms);
                *average = average.saturating_mul(4).saturating_add(record.duration_ms) / 5;
            }
            add_usage(&mut inner, &record);
        }
        let Some(path) = self.ledger.clone() else {
            return;
        };
        // File append and rotation are blocking; keep them off the runtime workers.
        let written = tokio::task::spawn_blocking(move || append(&path, &record))
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())));
        let mut inner = self.lock();
        match written {
            Ok(()) => {
                inner.ledger_error = None;
                inner.ledger_records += 1;
            }
            Err(error) => inner.ledger_error = Some(error.to_string()),
        }
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
                "path": self.ledger.as_deref(),
                "records": inner.ledger_records,
                "last_error": inner.ledger_error,
            },
        })
    }

    pub(super) fn ledger_receipt(&self) -> Value {
        let inner = self.lock();
        json!({
            "enabled": self.ledger.is_some(),
            "path": self.ledger.as_deref(),
            "records": inner.ledger_records,
            "last_error": inner.ledger_error,
        })
    }

    /// Render counters plus caller-supplied gauges in the Prometheus text format (0.0.4).
    #[allow(clippy::too_many_lines)]
    pub(super) fn render_prometheus(&self, gauges: &[Gauge]) -> String {
        let mut out = String::new();
        let inner = self.lock();
        let family = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        let task_series = |out: &mut String, name: &str, pick: &dyn Fn(&TaskCounters) -> f64| {
            for ((model, backend, task, outcome), counters) in &inner.tasks {
                let _ = writeln!(
                    out,
                    "{name}{{model=\"{}\",backend=\"{}\",task=\"{}\",outcome=\"{}\"}} {}",
                    escape(model),
                    escape(backend),
                    escape(task),
                    escape(outcome),
                    pick(counters)
                );
            }
        };
        #[allow(clippy::cast_precision_loss)]
        let seconds = |ms: u64| ms as f64 / 1000.0;
        #[allow(clippy::cast_precision_loss)]
        let float = |value: u64| value as f64;

        family(
            &mut out,
            "freellama_tasks_total",
            "counter",
            "Managed tasks finished, by outcome.",
        );
        task_series(&mut out, "freellama_tasks_total", &|c| float(c.count));
        family(
            &mut out,
            "freellama_prompt_tokens_total",
            "counter",
            "Prompt tokens reported by Ollama.",
        );
        task_series(&mut out, "freellama_prompt_tokens_total", &|c| {
            float(c.prompt_tokens)
        });
        family(
            &mut out,
            "freellama_output_tokens_total",
            "counter",
            "Output tokens reported by Ollama.",
        );
        task_series(&mut out, "freellama_output_tokens_total", &|c| {
            float(c.output_tokens)
        });
        family(
            &mut out,
            "freellama_task_seconds_total",
            "counter",
            "Wall time of managed tasks, queueing included.",
        );
        task_series(&mut out, "freellama_task_seconds_total", &|c| {
            seconds(c.duration_ms)
        });
        family(
            &mut out,
            "freellama_queue_wait_seconds_total",
            "counter",
            "Time managed tasks spent waiting for admission.",
        );
        task_series(&mut out, "freellama_queue_wait_seconds_total", &|c| {
            seconds(c.queue_wait_ms)
        });
        family(
            &mut out,
            "freellama_load_seconds_total",
            "counter",
            "Model load time reported by Ollama.",
        );
        task_series(&mut out, "freellama_load_seconds_total", &|c| {
            seconds(c.load_ms)
        });

        family(
            &mut out,
            "freellama_raw_requests_total",
            "counter",
            "Raw passthrough requests, by kind and outcome.",
        );
        for ((kind, outcome), count) in &inner.raw {
            let _ = writeln!(
                out,
                "freellama_raw_requests_total{{kind=\"{}\",outcome=\"{}\"}} {count}",
                escape(kind),
                escape(outcome)
            );
        }
        family(
            &mut out,
            "freellama_evictions_total",
            "counter",
            "Idle models unloaded to make room.",
        );
        for (backend, count) in &inner.evictions {
            let _ = writeln!(
                out,
                "freellama_evictions_total{{backend=\"{}\"}} {count}",
                escape(backend)
            );
        }
        family(
            &mut out,
            "freellama_circuit_open_total",
            "counter",
            "Times an upstream circuit breaker opened.",
        );
        for (backend, count) in &inner.breaker_opens {
            let _ = writeln!(
                out,
                "freellama_circuit_open_total{{backend=\"{}\"}} {count}",
                escape(backend)
            );
        }
        family(
            &mut out,
            "freellama_concurrency_limit_changes_total",
            "counter",
            "Adaptive concurrency adjustments.",
        );
        for ((backend, direction), count) in &inner.limit_changes {
            let _ = writeln!(
                out,
                "freellama_concurrency_limit_changes_total{{backend=\"{}\",direction=\"{}\"}} {count}",
                escape(backend),
                escape(direction)
            );
        }
        drop(inner);

        let mut current: Option<&str> = None;
        for gauge in gauges {
            if current != Some(gauge.name) {
                family(&mut out, gauge.name, gauge.kind, gauge.help);
                current = Some(gauge.name);
            }
            let labels = gauge
                .labels
                .iter()
                .map(|(key, value)| format!("{key}=\"{}\"", escape(value)))
                .collect::<Vec<_>>()
                .join(",");
            if labels.is_empty() {
                let _ = writeln!(out, "{} {}", gauge.name, gauge.value);
            } else {
                let _ = writeln!(out, "{}{{{labels}}} {}", gauge.name, gauge.value);
            }
        }
        family(
            &mut out,
            "freellama_uptime_seconds",
            "gauge",
            "Seconds since this FreeLlama process started.",
        );
        let _ = writeln!(
            out,
            "freellama_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );
        out
    }
}

/// One sample of a gauge; consecutive samples with the same name form one metric family.
pub(super) struct Gauge {
    pub(super) name: &'static str,
    pub(super) help: &'static str,
    pub(super) kind: &'static str,
    pub(super) labels: Vec<(&'static str, String)>,
    pub(super) value: f64,
}

impl Gauge {
    pub(super) fn new(name: &'static str, help: &'static str, value: f64) -> Self {
        Self {
            name,
            help,
            kind: "gauge",
            labels: Vec::new(),
            value,
        }
    }

    #[must_use]
    pub(super) fn label(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.labels.push((key, value.into()));
        self
    }

    #[must_use]
    pub(super) fn counter(mut self) -> Self {
        self.kind = "counter";
        self
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn add_usage(inner: &mut Inner, record: &TaskRecord) {
    inner
        .usage
        .entry((civil_day(unix_day(record.ts)), record.model.clone()))
        .or_default()
        .add(record);
    // Days sort lexically as ISO dates; drop the oldest beyond the retention bound.
    while inner
        .usage
        .keys()
        .map(|(day, _)| day)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        > MAX_USAGE_DAYS
    {
        let Some(oldest) = inner.usage.keys().next().map(|(day, _)| day.clone()) else {
            break;
        };
        inner.usage.retain(|(day, _), _| *day != oldest);
    }
}

fn append(path: &Path, record: &TaskRecord) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= LEDGER_MAX_BYTES) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        std::fs::rename(path, rotated)?;
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// Read ledger records back; malformed lines are skipped, not fatal.
fn replay(path: &Path) -> std::io::Result<Vec<TaskRecord>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let reader = std::io::BufReader::new(std::io::Read::take(file, LEDGER_MAX_BYTES));
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<TaskRecord>(&line).ok())
        .collect())
}

pub(super) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn unix_day(seconds: u64) -> u64 {
    seconds / 86_400
}

/// `YYYY-MM-DD` (UTC) for a day number since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_day(days: u64) -> String {
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Inverse of `civil_day`.
fn day_number(day: &str) -> Option<u64> {
    let mut parts = day.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    u64::try_from(era * 146_097 + doe - 719_468).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(model: &str, ts: u64, outcome: &str) -> TaskRecord {
        TaskRecord {
            ts,
            model: model.into(),
            backend: "gpu".into(),
            task: "coding".into(),
            priority: "normal".into(),
            outcome: outcome.into(),
            status: if outcome == "ok" { 200 } else { 503 },
            prompt_tokens: 100,
            output_tokens: 20,
            duration_ms: 1500,
            queue_wait_ms: 250,
            load_ms: 0,
            output_tokens_per_second: Some(40.0),
        }
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(civil_day(0), "1970-01-01");
        assert_eq!(civil_day(20_725), "2026-09-29");
        for day in [0, 59, 60, 365, 10_957, 20_725, 30_000] {
            assert_eq!(day_number(&civil_day(day)), Some(day));
        }
    }

    #[tokio::test]
    async fn usage_totals_group_by_day_and_model_and_count_errors() {
        let telemetry = Telemetry::default();
        let now = now_seconds();
        telemetry.record_task(record("a", now, "ok")).await;
        telemetry.record_task(record("a", now, "error")).await;
        telemetry.record_task(record("b", now, "ok")).await;
        telemetry
            .record_task(record("b", now - 10 * 86_400, "ok"))
            .await;
        let usage = telemetry.usage(7);
        assert_eq!(usage["totals"]["tasks"], 3);
        assert_eq!(usage["totals"]["errors"], 1);
        assert_eq!(usage["by_model"]["a"]["prompt_tokens"], 200);
        assert_eq!(usage["by_day"].as_array().unwrap().len(), 1);
        assert_eq!(telemetry.usage(30)["totals"]["tasks"], 4);
    }

    #[tokio::test]
    async fn ledger_survives_a_restart_and_skips_garbage_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage.jsonl");
        let telemetry = Telemetry::new(Some(path.clone()));
        telemetry
            .record_task(record("a", now_seconds(), "ok"))
            .await;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let restarted = Telemetry::new(Some(path));
        let usage = restarted.usage(1);
        assert_eq!(usage["totals"]["tasks"], 1);
        assert_eq!(usage["ledger"]["records"], 1);
        assert!(usage["ledger"]["last_error"].is_null());
    }

    #[tokio::test]
    async fn prometheus_text_has_typed_families_and_escaped_labels() {
        let telemetry = Telemetry::default();
        telemetry
            .record_task(record("we\"ird", now_seconds(), "ok"))
            .await;
        telemetry.record_raw("generation", "rejected_limit");
        let text = telemetry.render_prometheus(&[
            Gauge::new("freellama_queue_depth", "Waiting tasks.", 2.0).label("backend", "gpu"),
            Gauge::new("freellama_queue_depth", "Waiting tasks.", 0.0).label("backend", "cpu"),
        ]);
        assert!(text.contains("# TYPE freellama_tasks_total counter"));
        assert!(text.contains("model=\"we\\\"ird\""));
        assert!(text.contains(
            "freellama_raw_requests_total{kind=\"generation\",outcome=\"rejected_limit\"} 1"
        ));
        assert_eq!(
            text.matches("# TYPE freellama_queue_depth gauge").count(),
            1
        );
        assert!(text.contains("freellama_queue_depth{backend=\"cpu\"} 0"));
    }
}
