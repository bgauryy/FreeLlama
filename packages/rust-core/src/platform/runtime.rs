//! Live-tunable settings, the upstream circuit breaker, and adaptive concurrency.
//!
//! Precedence for every tunable, strongest first: an explicit `PlatformConfig`/CLI value, the
//! matching `FREELLAMA_*` environment variable, the runtime config file, then the built-in
//! default. The file is re-read when it changes (`serve` polls it) or on
//! `POST /_freellama/v1/config/reload`; values pinned by the CLI or environment stay pinned, and
//! `GET /_freellama/v1/config` reports each value with the source that set it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant, SystemTime},
};

use super::admission::AdmissionPool;
use super::telemetry::Telemetry;

/// Where adaptive concurrency applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptiveMode {
    Off,
    /// Only the optional CPU backend, where oversubscription shows up as throughput collapse.
    Cpu,
    /// Both backends.
    All,
}

/// Who owns `num_ctx` on managed requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    /// Always send the sized `num_ctx` (the pre-alignment behaviour).
    Explicit,
    /// Leave `num_ctx` unset when Ollama's own default for that model is known, covers the
    /// request, and is not above `auto_context_max`. The runner then stays in Ollama's automatic
    /// mode: raw clients share it without reloads and Ollama keeps its OOM shrink-and-retry.
    OllamaDefault,
}

/// The runtime config file. Every field is optional; unknown fields are rejected so a typo is an
/// error at reload instead of a silently ignored setting.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFile {
    pub max_concurrent_tasks: Option<usize>,
    pub cpu_max_concurrent_tasks: Option<usize>,
    pub max_queued_tasks: Option<usize>,
    pub cpu_max_queued_tasks: Option<usize>,
    pub max_queue_wait_seconds: Option<u64>,
    pub raw_queue_wait_seconds: Option<u64>,
    pub raw_max_concurrent_requests: Option<usize>,
    pub pinned_models: Option<Vec<String>>,
    pub evict_idle_models: Option<bool>,
    /// Per-model multiplier on the cost of unloading it (default 1): raise it for a model that is
    /// expensive to lose, lower it for one that is cheap to reload.
    pub eviction_costs: Option<BTreeMap<String, f64>>,
    pub adaptive_concurrency: Option<AdaptiveMode>,
    pub context_mode: Option<ContextMode>,
    pub ollama_default_context: Option<u64>,
    pub auto_context_max: Option<u64>,
    pub breaker_failures: Option<u32>,
    pub breaker_cooldown_seconds: Option<u64>,
}

impl RuntimeFile {
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or is not valid TOML for this schema.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
}

/// Values set by the operator at startup (CLI flags or an embedding application). They pin the
/// setting: the file cannot override them.
#[derive(Debug, Clone, Default)]
pub struct PinnedTunables {
    pub max_concurrent_tasks: Option<usize>,
    pub cpu_max_concurrent_tasks: Option<usize>,
    pub max_queued_tasks: Option<usize>,
    pub cpu_max_queued_tasks: Option<usize>,
    pub max_queue_wait: Option<Duration>,
    pub raw_max_concurrent_requests: Option<usize>,
    pub raw_queue_wait: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tunables {
    pub max_concurrent_tasks: usize,
    pub cpu_max_concurrent_tasks: usize,
    pub max_queued_tasks: usize,
    pub cpu_max_queued_tasks: usize,
    /// Milliseconds, so sub-second waits set by an embedding application survive.
    pub max_queue_wait_ms: u64,
    pub raw_queue_wait_seconds: u64,
    pub raw_max_concurrent_requests: usize,
    pub pinned_models: BTreeSet<String>,
    pub evict_idle_models: bool,
    pub eviction_costs: BTreeMap<String, f64>,
    pub adaptive_concurrency: AdaptiveMode,
    pub context_mode: ContextMode,
    pub ollama_default_context: Option<u64>,
    pub auto_context_max: u64,
    pub breaker_failures: u32,
    pub breaker_cooldown_seconds: u64,
    /// Setting name -> `cli`, `env`, `file`, `default`, or `ollama_num_parallel`.
    #[serde(skip)]
    pub sources: BTreeMap<&'static str, &'static str>,
}

impl Tunables {
    pub(super) fn max_queue_wait(&self) -> Duration {
        Duration::from_millis(self.max_queue_wait_ms.max(1))
    }

    pub(super) fn raw_queue_wait(&self) -> Duration {
        Duration::from_secs(self.raw_queue_wait_seconds)
    }

    pub(super) fn breaker_cooldown(&self) -> Duration {
        Duration::from_secs(self.breaker_cooldown_seconds.max(1))
    }

    pub(super) fn receipt(&self) -> Value {
        let values = serde_json::to_value(self).unwrap_or_default();
        let settings = values
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, value)| {
                (
                    name.clone(),
                    json!({
                        "value": value,
                        "source": self.sources.get(name.as_str()).copied().unwrap_or("default"),
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        Value::Object(settings)
    }
}

#[allow(clippy::too_many_lines)] // One `pick!` per setting keeps the precedence auditable.
/// Resolve every tunable. `env` is injectable so precedence is testable without mutating the
/// process environment (Rust 2024 makes that `unsafe`, which this crate denies).
pub(super) fn resolve(
    pinned: &PinnedTunables,
    file: &RuntimeFile,
    num_parallel: u64,
    env: &dyn Fn(&str) -> Option<String>,
) -> Tunables {
    let mut sources = BTreeMap::new();
    let parse = |name: &str| env(name).map(|value| value.trim().to_owned());
    macro_rules! pick {
        ($key:literal, $pinned:expr, $env:expr, $file:expr, $default:expr, $default_source:expr) => {{
            if let Some(value) = $pinned {
                sources.insert($key, "cli");
                value
            } else if let Some(value) = $env {
                sources.insert($key, "env");
                value
            } else if let Some(value) = $file {
                sources.insert($key, "file");
                value
            } else {
                sources.insert($key, $default_source);
                $default
            }
        }};
    }
    let number = |name: &str| parse(name).and_then(|value| value.parse::<u64>().ok());
    let positive = |name: &str| number(name).filter(|value| *value > 0);
    let usize_positive = |name: &str| positive(name).and_then(|value| usize::try_from(value).ok());
    let boolean = |name: &str| {
        parse(name).and_then(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
    };
    let enumerated =
        |name: &str| parse(name).map(|value| Value::String(value.to_ascii_lowercase()));
    // Two units per ordinary chat, so the default admits one chat per Ollama parallel slot.
    let parallel_units = usize::try_from(num_parallel.max(1).saturating_mul(2)).unwrap_or(2);

    let max_concurrent_tasks = pick!(
        "max_concurrent_tasks",
        pinned.max_concurrent_tasks,
        usize_positive("FREELLAMA_MAX_CONCURRENT_TASKS"),
        file.max_concurrent_tasks.filter(|value| *value > 0),
        parallel_units,
        "ollama_num_parallel"
    );
    let cpu_max_concurrent_tasks = pick!(
        "cpu_max_concurrent_tasks",
        pinned.cpu_max_concurrent_tasks,
        usize_positive("FREELLAMA_CPU_MAX_CONCURRENT_TASKS"),
        file.cpu_max_concurrent_tasks.filter(|value| *value > 0),
        1,
        "default"
    );
    let max_queued_tasks = pick!(
        "max_queued_tasks",
        pinned.max_queued_tasks,
        usize_positive("FREELLAMA_MAX_QUEUED_TASKS"),
        file.max_queued_tasks.filter(|value| *value > 0),
        16,
        "default"
    );
    let cpu_max_queued_tasks = pick!(
        "cpu_max_queued_tasks",
        pinned.cpu_max_queued_tasks,
        usize_positive("FREELLAMA_CPU_MAX_QUEUED_TASKS"),
        file.cpu_max_queued_tasks.filter(|value| *value > 0),
        8,
        "default"
    );
    let max_queue_wait_ms = pick!(
        "max_queue_wait_ms",
        pinned
            .max_queue_wait
            .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX).max(1)),
        positive("FREELLAMA_MAX_QUEUE_WAIT_SECONDS").map(|seconds| seconds.saturating_mul(1000)),
        file.max_queue_wait_seconds
            .filter(|value| *value > 0)
            .map(|seconds| seconds.saturating_mul(1000)),
        120_000,
        "default"
    );
    let raw_queue_wait_seconds = pick!(
        "raw_queue_wait_seconds",
        pinned.raw_queue_wait.map(|wait| wait.as_secs()),
        number("FREELLAMA_RAW_QUEUE_WAIT_SECONDS"),
        file.raw_queue_wait_seconds,
        10,
        "default"
    );
    let raw_max_concurrent_requests = pick!(
        "raw_max_concurrent_requests",
        pinned.raw_max_concurrent_requests,
        usize_positive("FREELLAMA_RAW_MAX_CONCURRENT_REQUESTS"),
        file.raw_max_concurrent_requests.filter(|value| *value > 0),
        1,
        "default"
    );
    let pinned_models = pick!(
        "pinned_models",
        None::<BTreeSet<String>>,
        parse("FREELLAMA_PINNED_MODELS").map(|value| value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()),
        file.pinned_models
            .clone()
            .map(|models| models.into_iter().collect()),
        BTreeSet::new(),
        "default"
    );
    let evict_idle_models = pick!(
        "evict_idle_models",
        None::<bool>,
        boolean("FREELLAMA_EVICT_IDLE_MODELS"),
        file.evict_idle_models,
        true,
        "default"
    );
    let eviction_costs = pick!(
        "eviction_costs",
        None::<BTreeMap<String, f64>>,
        None::<BTreeMap<String, f64>>,
        file.eviction_costs.clone().map(|costs| costs
            .into_iter()
            .filter(|(_, weight)| weight.is_finite() && *weight >= 0.0)
            .collect()),
        BTreeMap::new(),
        "default"
    );
    let adaptive_concurrency = pick!(
        "adaptive_concurrency",
        None::<AdaptiveMode>,
        enumerated("FREELLAMA_ADAPTIVE_CONCURRENCY")
            .and_then(|value| serde_json::from_value(value).ok()),
        file.adaptive_concurrency,
        AdaptiveMode::Cpu,
        "default"
    );
    let context_mode = pick!(
        "context_mode",
        None::<ContextMode>,
        enumerated("FREELLAMA_CONTEXT_MODE").and_then(|value| serde_json::from_value(value).ok()),
        file.context_mode,
        ContextMode::OllamaDefault,
        "default"
    );
    let ollama_default_context = pick!(
        "ollama_default_context",
        None::<Option<u64>>,
        positive("FREELLAMA_OLLAMA_DEFAULT_CONTEXT").map(Some),
        file.ollama_default_context
            .filter(|value| *value > 0)
            .map(Some),
        None,
        "probe"
    );
    let auto_context_max = pick!(
        "auto_context_max",
        None::<u64>,
        positive("FREELLAMA_AUTO_CONTEXT_MAX"),
        file.auto_context_max.filter(|value| *value > 0),
        32_768,
        "default"
    );
    let breaker_failures = pick!(
        "breaker_failures",
        None::<u32>,
        number("FREELLAMA_BREAKER_FAILURES").and_then(|value| u32::try_from(value).ok()),
        file.breaker_failures,
        3,
        "default"
    );
    let breaker_cooldown_seconds = pick!(
        "breaker_cooldown_seconds",
        None::<u64>,
        positive("FREELLAMA_BREAKER_COOLDOWN_SECONDS"),
        file.breaker_cooldown_seconds.filter(|value| *value > 0),
        15,
        "default"
    );
    Tunables {
        max_concurrent_tasks,
        cpu_max_concurrent_tasks,
        max_queued_tasks,
        cpu_max_queued_tasks,
        max_queue_wait_ms,
        raw_queue_wait_seconds,
        raw_max_concurrent_requests,
        pinned_models,
        evict_idle_models,
        eviction_costs,
        adaptive_concurrency,
        context_mode,
        ollama_default_context,
        auto_context_max,
        breaker_failures,
        breaker_cooldown_seconds,
        sources,
    }
}

/// Process-environment lookup for `resolve`.
pub(super) fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Live tunables plus what is needed to re-resolve them.
#[derive(Clone)]
pub(super) struct RuntimeSettings {
    current: Arc<std::sync::RwLock<Tunables>>,
    pinned: Arc<PinnedTunables>,
    file: Option<Arc<PathBuf>>,
    num_parallel: u64,
    last_reload: Arc<StdMutex<ReloadStatus>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ReloadStatus {
    #[serde(skip)]
    modified: Option<SystemTime>,
    reloads: u64,
    last_error: Option<String>,
    last_reload_unix: Option<u64>,
}

impl RuntimeSettings {
    pub(super) fn new(
        pinned: PinnedTunables,
        file: Option<PathBuf>,
        num_parallel: u64,
    ) -> anyhow::Result<Self> {
        let parsed = match file.as_deref() {
            Some(path) if path.exists() => RuntimeFile::load(path)
                .map_err(|error| anyhow::anyhow!("runtime config {}: {error}", path.display()))?,
            _ => RuntimeFile::default(),
        };
        let modified = file
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok()?.modified().ok());
        let tunables = resolve(&pinned, &parsed, num_parallel, &process_env);
        Ok(Self {
            current: Arc::new(std::sync::RwLock::new(tunables)),
            pinned: Arc::new(pinned),
            file: file.map(Arc::new),
            num_parallel,
            last_reload: Arc::new(StdMutex::new(ReloadStatus {
                modified,
                ..ReloadStatus::default()
            })),
        })
    }

    pub(super) fn get(&self) -> Tunables {
        self.current.read().expect("tunables poisoned").clone()
    }

    pub(super) fn file(&self) -> Option<&Path> {
        self.file.as_deref().map(PathBuf::as_path)
    }

    /// Re-read the file when its modification time changed (or always, with `force`).
    /// Returns `Ok(Some(new))` when the effective values changed. A bad file keeps the previous
    /// values and records the error.
    pub(super) fn reload(&self, force: bool) -> Result<Option<Tunables>, String> {
        let Some(path) = self.file.as_deref() else {
            return if force {
                Err("no runtime config file configured (set --runtime-config or FREELLAMA_RUNTIME_CONFIG)".into())
            } else {
                Ok(None)
            };
        };
        let modified = std::fs::metadata(path.as_path())
            .ok()
            .and_then(|meta| meta.modified().ok());
        {
            let status = self.last_reload.lock().expect("reload status poisoned");
            if !force && status.modified == modified {
                return Ok(None);
            }
        }
        let parsed = if path.exists() {
            RuntimeFile::load(path)
        } else {
            Ok(RuntimeFile::default())
        };
        let mut status = self.last_reload.lock().expect("reload status poisoned");
        status.modified = modified;
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                let message = error.to_string();
                status.last_error = Some(message.clone());
                return Err(message);
            }
        };
        status.last_error = None;
        status.reloads += 1;
        status.last_reload_unix = Some(super::telemetry::now_seconds());
        drop(status);
        let next = resolve(&self.pinned, &parsed, self.num_parallel, &process_env);
        let mut current = self.current.write().expect("tunables poisoned");
        if *current == next {
            return Ok(None);
        }
        *current = next.clone();
        Ok(Some(next))
    }

    pub(super) fn receipt(&self) -> Value {
        let status = self
            .last_reload
            .lock()
            .expect("reload status poisoned")
            .clone();
        json!({
            "file": self.file.as_deref(),
            "precedence": ["cli", "env", "file", "default"],
            "reload": status,
            "settings": self.get().receipt(),
        })
    }
}

/// Per-backend circuit breaker around upstream failures (transport errors, 500/502/504).
///
/// After `breaker_failures` consecutive failures the circuit opens for `breaker_cooldown_seconds`:
/// managed tasks fail fast with 503 and `Retry-After` instead of each queueing and then waiting
/// out a dead or crash-looping Ollama. After the cooldown one request is let through; its result
/// closes or re-opens the circuit.
#[derive(Clone, Default)]
pub(super) struct Breakers {
    inner: Arc<StdMutex<HashMap<String, BreakerState>>>,
}

#[derive(Debug, Default)]
struct BreakerState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    probe_in_flight: bool,
    opened: u64,
}

/// The single request allowed through a half-open circuit. Dropping it without a recorded
/// outcome (the task failed before reaching Ollama) frees the probe slot for the next request.
pub(super) struct BreakerProbe {
    breakers: Breakers,
    backend: String,
}

impl Drop for BreakerProbe {
    fn drop(&mut self) {
        if let Some(state) = self
            .breakers
            .inner
            .lock()
            .expect("breaker poisoned")
            .get_mut(&self.backend)
        {
            state.probe_in_flight = false;
        }
    }
}

impl Breakers {
    /// `Err(remaining)` while the circuit is open; `Ok(Some(probe))` for the one request allowed
    /// through after the cooldown.
    pub(super) fn check(&self, backend: &str) -> Result<Option<BreakerProbe>, Duration> {
        let mut map = self.inner.lock().expect("breaker poisoned");
        let state = map.entry(backend.to_owned()).or_default();
        let Some(until) = state.open_until else {
            return Ok(None);
        };
        let now = Instant::now();
        if now < until {
            return Err(until - now);
        }
        if state.probe_in_flight {
            return Err(Duration::from_secs(1));
        }
        state.probe_in_flight = true;
        Ok(Some(BreakerProbe {
            breakers: self.clone(),
            backend: backend.to_owned(),
        }))
    }

    /// Returns true when this failure opened the circuit.
    pub(super) fn record(
        &self,
        backend: &str,
        success: bool,
        threshold: u32,
        cooldown: Duration,
    ) -> bool {
        let mut map = self.inner.lock().expect("breaker poisoned");
        let state = map.entry(backend.to_owned()).or_default();
        let was_probe = state.open_until.is_some();
        if success {
            state.consecutive_failures = 0;
            state.open_until = None;
            return false;
        }
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if threshold > 0 && (was_probe || state.consecutive_failures >= threshold) {
            state.open_until = Some(Instant::now() + cooldown);
            state.opened += 1;
            return true;
        }
        false
    }

    pub(super) fn receipt(&self) -> Value {
        let map = self.inner.lock().expect("breaker poisoned");
        let now = Instant::now();
        map.iter()
            .map(|(backend, state)| {
                (
                    backend.clone(),
                    json!({
                        "state": match state.open_until {
                            Some(until) if now < until => "open",
                            Some(_) => "half_open",
                            None => "closed",
                        },
                        "consecutive_failures": state.consecutive_failures,
                        "retry_after_seconds": state.open_until
                            .filter(|until| now < *until)
                            .map(|until| (until - now).as_secs().max(1)),
                        "times_opened": state.opened,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

/// Additive-increase / multiplicative-decrease control of one backend's admission limit.
///
/// A bad completion (upstream failure, host memory pressure, or output throughput below 70% of
/// that model's healthy baseline) halves the limit, at most once per `DECREASE_SPACING`. Five
/// healthy completions in a row raise it by one unit, up to the configured ceiling. The fixed
/// setting stays the ceiling, so this can only make `FreeLlama` more conservative.
#[derive(Clone, Default)]
pub(super) struct AdaptiveLimiter {
    inner: Arc<StdMutex<AdaptiveState>>,
}

#[derive(Debug, Default)]
struct AdaptiveState {
    baseline_tps: HashMap<String, (f64, u32)>,
    healthy_streak: u32,
    last_decrease: Option<Instant>,
    last_reason: Option<String>,
}

pub(super) const DECREASE_SPACING: Duration = Duration::from_secs(10);
const HEALTHY_STREAK_FOR_INCREASE: u32 = 5;
const THROUGHPUT_DROP_RATIO: f64 = 0.7;
const BASELINE_MIN_SAMPLES: u32 = 3;
const BASELINE_ALPHA: f64 = 0.2;

/// One finished task as the controller sees it.
pub(super) struct Completion<'a> {
    pub(super) model: &'a str,
    pub(super) failed: bool,
    pub(super) memory_pressure: bool,
    pub(super) output_tokens_per_second: Option<f64>,
}

impl AdaptiveLimiter {
    /// Feed one completion; returns `Some((direction, new_limit))` when the limit changed.
    pub(super) fn observe(
        &self,
        pool: &AdmissionPool,
        completion: &Completion<'_>,
    ) -> Option<(&'static str, usize)> {
        let mut state = self.inner.lock().expect("adaptive state poisoned");
        let slow = completion.output_tokens_per_second.and_then(|tps| {
            let (baseline, samples) = state.baseline_tps.get(completion.model).copied()?;
            (samples >= BASELINE_MIN_SAMPLES && tps < baseline * THROUGHPUT_DROP_RATIO)
                .then_some((tps, baseline))
        });
        let reason = if completion.failed {
            Some("upstream_failure".to_owned())
        } else if completion.memory_pressure {
            Some("host_memory_pressure".to_owned())
        } else {
            slow.map(|(tps, baseline)| {
                format!("throughput {tps:.1} tok/s below 70% of baseline {baseline:.1}")
            })
        };
        if let Some(reason) = reason {
            state.healthy_streak = 0;
            if state
                .last_decrease
                .is_some_and(|last| last.elapsed() < DECREASE_SPACING)
            {
                return None;
            }
            let current = pool.total();
            let next = pool.set_limit((current / 2).max(1));
            state.last_decrease = Some(Instant::now());
            state.last_reason = Some(reason);
            return (next < current).then_some(("decrease", next));
        }
        if let Some(tps) = completion.output_tokens_per_second.filter(|tps| *tps > 0.0) {
            let entry = state
                .baseline_tps
                .entry(completion.model.to_owned())
                .or_insert((tps, 0));
            entry.0 = if entry.1 == 0 {
                tps
            } else {
                entry.0 * (1.0 - BASELINE_ALPHA) + tps * BASELINE_ALPHA
            };
            entry.1 = entry.1.saturating_add(1);
        }
        state.healthy_streak += 1;
        if state.healthy_streak >= HEALTHY_STREAK_FOR_INCREASE && pool.total() < pool.ceiling() {
            state.healthy_streak = 0;
            let current = pool.total();
            let next = pool.set_limit(current + 1);
            return (next > current).then_some(("increase", next));
        }
        None
    }

    pub(super) fn receipt(&self, pool: &AdmissionPool, enabled: bool) -> Value {
        let state = self.inner.lock().expect("adaptive state poisoned");
        json!({
            "enabled": enabled,
            "limit": pool.total(),
            "ceiling": pool.ceiling(),
            "healthy_streak": state.healthy_streak,
            "last_decrease_reason": state.last_reason,
            "baseline_output_tokens_per_second": state.baseline_tps.iter()
                .map(|(model, (tps, samples))| (model.clone(), json!({"tps": tps, "samples": samples})))
                .collect::<serde_json::Map<_, _>>(),
        })
    }
}

/// Estimated seconds until capacity frees up: waiting tasks ahead, times the recent average
/// service time, spread over the current limit. Clamped to 1..=120 for `Retry-After`.
pub(super) fn retry_after_seconds(
    queue_depth: usize,
    limit: usize,
    average_task_ms: Option<u64>,
) -> u64 {
    let per_task = average_task_ms.unwrap_or(5_000).max(250);
    let ahead = u64::try_from(queue_depth.saturating_add(1)).unwrap_or(u64::MAX);
    let lanes = u64::try_from(limit.max(1)).unwrap_or(1);
    (ahead.saturating_mul(per_task) / lanes)
        .div_ceil(1000)
        .clamp(1, 120)
}

/// Record adaptive changes in telemetry; small helper shared by managed and batch paths.
pub(super) fn note_limit_change(
    telemetry: &Telemetry,
    backend: &str,
    change: Option<(&str, usize)>,
) {
    if let Some((direction, limit)) = change {
        telemetry.record_limit_change(backend, direction);
        eprintln!("adaptive concurrency: {backend} limit {direction}d to {limit} units");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn precedence_is_cli_then_env_then_file_then_default() {
        let file = RuntimeFile {
            max_concurrent_tasks: Some(6),
            cpu_max_concurrent_tasks: Some(3),
            max_queued_tasks: Some(40),
            ..RuntimeFile::default()
        };
        let pinned = PinnedTunables {
            max_concurrent_tasks: Some(8),
            ..PinnedTunables::default()
        };
        let env = env_from(&[("FREELLAMA_CPU_MAX_CONCURRENT_TASKS", "2")]);
        let tunables = resolve(&pinned, &file, 1, &env);
        assert_eq!(tunables.max_concurrent_tasks, 8);
        assert_eq!(tunables.sources["max_concurrent_tasks"], "cli");
        assert_eq!(tunables.cpu_max_concurrent_tasks, 2);
        assert_eq!(tunables.sources["cpu_max_concurrent_tasks"], "env");
        assert_eq!(tunables.max_queued_tasks, 40);
        assert_eq!(tunables.sources["max_queued_tasks"], "file");
        assert_eq!(tunables.cpu_max_queued_tasks, 8);
        assert_eq!(tunables.sources["cpu_max_queued_tasks"], "default");
    }

    #[test]
    fn gpu_default_follows_ollama_num_parallel() {
        let none = env_from(&[]);
        let tunables = resolve(
            &PinnedTunables::default(),
            &RuntimeFile::default(),
            3,
            &none,
        );
        assert_eq!(tunables.max_concurrent_tasks, 6);
        assert_eq!(
            tunables.sources["max_concurrent_tasks"],
            "ollama_num_parallel"
        );
        let single = resolve(
            &PinnedTunables::default(),
            &RuntimeFile::default(),
            1,
            &none,
        );
        assert_eq!(single.max_concurrent_tasks, 2);
    }

    #[test]
    fn enums_and_lists_parse_from_env_and_file() {
        let env = env_from(&[
            ("FREELLAMA_ADAPTIVE_CONCURRENCY", "ALL"),
            ("FREELLAMA_PINNED_MODELS", " a:1 , ,b:2"),
        ]);
        let file: RuntimeFile = toml::from_str(
            "context_mode = \"explicit\"\nadaptive_concurrency = \"off\"\nbreaker_failures = 0\n",
        )
        .unwrap();
        let tunables = resolve(&PinnedTunables::default(), &file, 1, &env);
        assert_eq!(tunables.adaptive_concurrency, AdaptiveMode::All);
        assert_eq!(tunables.context_mode, ContextMode::Explicit);
        assert_eq!(tunables.breaker_failures, 0);
        assert_eq!(
            tunables.pinned_models,
            BTreeSet::from(["a:1".to_owned(), "b:2".to_owned()])
        );
    }

    #[test]
    fn unknown_file_fields_are_rejected() {
        assert!(toml::from_str::<RuntimeFile>("max_concurent_tasks = 2").is_err());
    }

    #[test]
    fn reload_picks_up_file_edits_keeps_pins_and_survives_bad_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.toml");
        std::fs::write(&path, "max_queued_tasks = 5\n").unwrap();
        let settings = RuntimeSettings::new(
            PinnedTunables {
                cpu_max_queued_tasks: Some(2),
                ..PinnedTunables::default()
            },
            Some(path.clone()),
            1,
        )
        .unwrap();
        assert_eq!(settings.get().max_queued_tasks, 5);
        std::fs::write(&path, "max_queued_tasks = 9\ncpu_max_queued_tasks = 50\n").unwrap();
        let changed = settings.reload(true).unwrap().expect("values changed");
        assert_eq!(changed.max_queued_tasks, 9);
        assert_eq!(changed.cpu_max_queued_tasks, 2, "a CLI pin beats the file");
        std::fs::write(&path, "max_queued_tasks = \"nine\"\n").unwrap();
        assert!(settings.reload(true).is_err());
        assert_eq!(
            settings.get().max_queued_tasks,
            9,
            "a bad file keeps the last good values"
        );
        assert!(settings.receipt()["reload"]["last_error"].is_string());
    }

    #[test]
    fn breaker_opens_after_threshold_and_half_opens_after_cooldown() {
        let breakers = Breakers::default();
        let cooldown = Duration::from_millis(30);
        assert!(breakers.check("gpu").is_ok());
        assert!(!breakers.record("gpu", false, 2, cooldown));
        assert!(breakers.record("gpu", false, 2, cooldown));
        assert!(breakers.check("gpu").is_err());
        std::thread::sleep(Duration::from_millis(40));
        let probe = breakers.check("gpu").expect("one probe after the cooldown");
        assert!(probe.is_some());
        assert!(breakers.check("gpu").is_err(), "only one probe at a time");
        drop(probe);
        let probe = breakers
            .check("gpu")
            .expect("an abandoned probe frees the slot");
        assert!(
            breakers.record("gpu", false, 2, cooldown),
            "a failed probe re-opens"
        );
        drop(probe);
        assert!(breakers.check("gpu").is_err());
        std::thread::sleep(Duration::from_millis(40));
        let probe = breakers.check("gpu").unwrap();
        assert!(!breakers.record("gpu", true, 2, cooldown));
        drop(probe);
        assert!(breakers.check("gpu").is_ok());
        assert_eq!(breakers.receipt()["gpu"]["state"], "closed");
    }

    #[test]
    fn breaker_threshold_zero_disables_it() {
        let breakers = Breakers::default();
        for _ in 0..10 {
            assert!(!breakers.record("cpu", false, 0, Duration::from_secs(1)));
        }
        assert!(breakers.check("cpu").is_ok());
    }

    #[test]
    fn adaptive_limit_halves_on_trouble_and_climbs_back_slowly() {
        let pool = AdmissionPool::new(8, 4);
        let limiter = AdaptiveLimiter::default();
        let healthy = |tps| Completion {
            model: "m",
            failed: false,
            memory_pressure: false,
            output_tokens_per_second: Some(tps),
        };
        for _ in 0..3 {
            assert_eq!(limiter.observe(&pool, &healthy(40.0)), None);
        }
        assert_eq!(
            limiter.observe(&pool, &healthy(10.0)),
            Some(("decrease", 4))
        );
        // A second bad sample inside the spacing window does not compound the cut.
        assert_eq!(limiter.observe(&pool, &healthy(10.0)), None);
        assert_eq!(pool.total(), 4);
        for _ in 0..4 {
            assert_eq!(limiter.observe(&pool, &healthy(40.0)), None);
        }
        assert_eq!(
            limiter.observe(&pool, &healthy(40.0)),
            Some(("increase", 5))
        );
        assert_eq!(pool.ceiling(), 8);
    }

    #[test]
    fn pressure_and_failures_count_as_bad_even_without_throughput() {
        let pool = AdmissionPool::new(4, 4);
        let limiter = AdaptiveLimiter::default();
        let change = limiter.observe(
            &pool,
            &Completion {
                model: "m",
                failed: false,
                memory_pressure: true,
                output_tokens_per_second: None,
            },
        );
        assert_eq!(change, Some(("decrease", 2)));
        assert_eq!(
            limiter.receipt(&pool, true)["last_decrease_reason"],
            "host_memory_pressure"
        );
    }

    #[test]
    fn retry_after_scales_with_queue_and_is_bounded() {
        assert_eq!(retry_after_seconds(0, 2, Some(4_000)), 2);
        assert_eq!(retry_after_seconds(9, 2, Some(4_000)), 20);
        assert_eq!(retry_after_seconds(10_000, 1, Some(60_000)), 120);
        assert_eq!(retry_after_seconds(0, 4, None), 2);
    }
}
