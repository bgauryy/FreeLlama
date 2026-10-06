//! Tunable resolution: CLI, then environment, then file, then default.
//!
//! Only documented operator knobs have environment twins (queue wait, idle eviction, usage queue
//! capacity, scope limits and keep-alive tuning); every other tunable is CLI or runtime file only.

use std::collections::{BTreeMap, BTreeSet};

use super::{AdaptiveMode, ContextMode, PinnedTunables, RuntimeFile, Tunables, default_task_cost};

/// CPU-backend admission units when nothing else sets them.
const DEFAULT_CPU_MAX_CONCURRENT_TASKS: usize = 1;
/// Managed requests retained while the primary pool is saturated.
const DEFAULT_MAX_QUEUED_TASKS: usize = 16;
/// Managed requests retained while the CPU pool is saturated.
const DEFAULT_CPU_MAX_QUEUED_TASKS: usize = 8;

/// Resolve every tunable. `env` is injectable so precedence is testable without mutating the
/// process environment (Rust 2024 makes that `unsafe`, which this crate denies).
#[expect(
    clippy::too_many_lines,
    reason = "one pick! per setting keeps the precedence auditable"
)]
pub(super) fn resolve(
    pinned: &PinnedTunables,
    file: &RuntimeFile,
    num_parallel: u64,
    env: &dyn Fn(&str) -> Option<String>,
) -> Tunables {
    let mut sources = BTreeMap::new();
    let reader = EnvReader::new(env);
    macro_rules! pick {
        ($key:literal, $pinned:expr, $env:expr, $file:expr, $default:expr, $default_source:expr) => {{
            if let Some(value) = $pinned {
                sources.insert($key, "cli");
                value
            } else {
                let invalid_before = reader.invalid_count();
                if let Some(value) = $env {
                    sources.insert($key, "env");
                    value
                } else {
                    let env_invalid = reader.invalid_count() > invalid_before;
                    let (value, source) = match $file {
                        Some(value) => (value, "file"),
                        None => ($default, $default_source),
                    };
                    sources.insert($key, if env_invalid { "env_invalid" } else { source });
                    value
                }
            }
        }};
    }
    let number = |name: &str| reader.checked(name, |value| value.parse::<u64>().ok());
    let positive = |name: &str| {
        reader.checked(name, |value| {
            value.parse::<u64>().ok().filter(|value| *value > 0)
        })
    };
    let usize_positive = |name: &str| {
        reader.checked(name, |value| {
            value.parse::<usize>().ok().filter(|value| *value > 0)
        })
    };
    let boolean = |name: &str| {
        reader.checked(name, |value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
    };
    // Two units per ordinary chat, so the default admits one chat per Ollama parallel slot.
    let parallel_units = usize::try_from(num_parallel.max(1).saturating_mul(2)).unwrap_or(2);

    let max_concurrent_tasks = pick!(
        "max_concurrent_tasks",
        pinned.max_concurrent_tasks,
        None::<usize>,
        file.max_concurrent_tasks.filter(|value| *value > 0),
        parallel_units,
        "ollama_num_parallel"
    );
    let cpu_max_concurrent_tasks = pick!(
        "cpu_max_concurrent_tasks",
        pinned.cpu_max_concurrent_tasks,
        None::<usize>,
        file.cpu_max_concurrent_tasks.filter(|value| *value > 0),
        DEFAULT_CPU_MAX_CONCURRENT_TASKS,
        "default"
    );
    let max_queued_tasks = pick!(
        "max_queued_tasks",
        pinned.max_queued_tasks,
        None::<usize>,
        file.max_queued_tasks.filter(|value| *value > 0),
        DEFAULT_MAX_QUEUED_TASKS,
        "default"
    );
    let cpu_max_queued_tasks = pick!(
        "cpu_max_queued_tasks",
        pinned.cpu_max_queued_tasks,
        None::<usize>,
        file.cpu_max_queued_tasks.filter(|value| *value > 0),
        DEFAULT_CPU_MAX_QUEUED_TASKS,
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
        None::<u64>,
        file.raw_queue_wait_seconds,
        10,
        "default"
    );
    let raw_max_concurrent_requests = pick!(
        "raw_max_concurrent_requests",
        pinned.raw_max_concurrent_requests,
        None::<usize>,
        file.raw_max_concurrent_requests.filter(|value| *value > 0),
        1,
        "default"
    );
    let pinned_models = pick!(
        "pinned_models",
        None::<BTreeSet<String>>,
        None::<BTreeSet<String>>,
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
        None::<AdaptiveMode>,
        file.adaptive_concurrency,
        AdaptiveMode::Cpu,
        "default"
    );
    let mut task_costs: BTreeMap<_, _> =
        <super::super::TaskKind as clap::ValueEnum>::value_variants()
            .iter()
            .copied()
            .map(|task| (task, default_task_cost(task)))
            .collect();
    if let Some(overrides) = &file.task_costs {
        task_costs.extend(overrides.iter().map(|(task, cost)| (*task, *cost)));
        sources.insert("task_costs", "file");
    } else {
        sources.insert("task_costs", "default");
    }
    let context_mode = pick!(
        "context_mode",
        None::<ContextMode>,
        None::<ContextMode>,
        file.context_mode,
        ContextMode::OllamaDefault,
        "default"
    );
    let ollama_default_context = pick!(
        "ollama_default_context",
        None::<Option<u64>>,
        None::<Option<u64>>,
        file.ollama_default_context
            .filter(|value| *value > 0)
            .map(Some),
        None,
        "probe"
    );
    let auto_context_max = pick!(
        "auto_context_max",
        None::<u64>,
        None::<u64>,
        file.auto_context_max.filter(|value| *value > 0),
        32_768,
        "default"
    );
    let breaker_failures = pick!(
        "breaker_failures",
        None::<u32>,
        None::<u32>,
        file.breaker_failures,
        3,
        "default"
    );
    let breaker_cooldown_seconds = pick!(
        "breaker_cooldown_seconds",
        None::<u64>,
        None::<u64>,
        file.breaker_cooldown_seconds.filter(|value| *value > 0),
        15,
        "default"
    );
    let usage_queue_capacity = pick!(
        "usage_queue_capacity",
        None::<std::num::NonZeroUsize>,
        reader.checked("FREELLAMA_USAGE_QUEUE_CAPACITY", |value| value
            .parse::<std::num::NonZeroUsize>()
            .ok()),
        file.usage_queue_capacity,
        super::super::telemetry::LEDGER_QUEUE_CAPACITY,
        "default"
    );
    let mut scopes = pick!(
        "scopes",
        None::<super::super::ScopePolicy>,
        None::<super::super::ScopePolicy>,
        file.scopes.clone(),
        super::super::ScopePolicy::default(),
        "default"
    );
    let mut warming = pick!(
        "warming",
        None::<super::super::WarmingPolicy>,
        None::<super::super::WarmingPolicy>,
        file.warming.clone(),
        super::super::WarmingPolicy::default(),
        "default"
    );
    macro_rules! env_override {
        ($policy:ident, $field:ident, $name:literal, $parser:expr, $source:literal) => {
            if let Some(value) = $parser($name) {
                $policy.$field = value;
                sources.insert($source, "env");
            }
        };
    }
    env_override!(
        scopes,
        max_count,
        "FREELLAMA_SCOPE_MAX_COUNT",
        usize_positive,
        "scopes"
    );
    env_override!(
        scopes,
        max_messages,
        "FREELLAMA_SCOPE_MAX_MESSAGES",
        usize_positive,
        "scopes"
    );
    env_override!(
        scopes,
        max_bytes,
        "FREELLAMA_SCOPE_MAX_BYTES",
        usize_positive,
        "scopes"
    );
    env_override!(
        scopes,
        max_estimated_tokens,
        "FREELLAMA_SCOPE_MAX_ESTIMATED_TOKENS",
        positive,
        "scopes"
    );
    env_override!(
        scopes,
        total_max_bytes,
        "FREELLAMA_SCOPE_TOTAL_MAX_BYTES",
        usize_positive,
        "scopes"
    );
    env_override!(
        scopes,
        ttl_seconds,
        "FREELLAMA_SCOPE_TTL_SECONDS",
        positive,
        "scopes"
    );
    env_override!(
        warming,
        min_seconds,
        "FREELLAMA_KEEP_ALIVE_MIN_SECONDS",
        positive,
        "warming"
    );
    env_override!(
        warming,
        max_seconds,
        "FREELLAMA_KEEP_ALIVE_MAX_SECONDS",
        positive,
        "warming"
    );
    env_override!(
        warming,
        base_seconds,
        "FREELLAMA_KEEP_ALIVE_BASE_SECONDS",
        positive,
        "warming"
    );
    env_override!(
        warming,
        reuse_gain_seconds,
        "FREELLAMA_KEEP_ALIVE_REUSE_GAIN_SECONDS",
        number,
        "warming"
    );
    let finite = |name: &str| reader.checked(name, |value| value.parse::<f64>().ok());
    env_override!(
        warming,
        load_multiplier,
        "FREELLAMA_KEEP_ALIVE_LOAD_MULTIPLIER",
        finite,
        "warming"
    );
    env_override!(
        warming,
        pressure_factor,
        "FREELLAMA_KEEP_ALIVE_PRESSURE_FACTOR",
        finite,
        "warming"
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
        task_costs,
        context_mode,
        ollama_default_context,
        auto_context_max,
        usage_queue_capacity,
        scopes,
        warming,
        breaker_failures,
        breaker_cooldown_seconds,
        sources,
        env_warnings: reader.into_warnings(),
    }
}

/// Environment lookups for one resolution, remembering every set value that failed to parse.
struct EnvReader<'a> {
    env: &'a dyn Fn(&str) -> Option<String>,
    warnings: std::cell::RefCell<Vec<String>>,
}

impl<'a> EnvReader<'a> {
    fn new(env: &'a dyn Fn(&str) -> Option<String>) -> Self {
        Self {
            env,
            warnings: std::cell::RefCell::default(),
        }
    }

    /// The trimmed value, or `None` when the variable is unset.
    fn raw(&self, name: &str) -> Option<String> {
        (self.env)(name).map(|value| value.trim().to_owned())
    }

    /// Parse a set, non-empty value; a value that does not parse is recorded and ignored.
    fn checked<T>(&self, name: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
        let raw = self.raw(name).filter(|value| !value.is_empty())?;
        let parsed = parse(&raw);
        if parsed.is_none() {
            self.warnings
                .borrow_mut()
                .push(format!("{name}={raw:?} is invalid and was ignored"));
        }
        parsed
    }

    fn invalid_count(&self) -> usize {
        self.warnings.borrow().len()
    }

    fn into_warnings(self) -> Vec<String> {
        self.warnings.into_inner()
    }
}

/// Process-environment lookup for `resolve`.
pub(super) fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}
