//! Live-tunable settings, the upstream circuit breaker, and adaptive concurrency.
//!
//! Precedence for every tunable, strongest first: an explicit `PlatformConfig`/CLI value, the
//! matching `FREELLAMA_*` environment variable where one exists (queue wait, idle eviction, usage
//! queue capacity, scope and keep-alive limits), the runtime config file, then the built-in
//! default. The file is re-read when it changes (`serve` polls it) or on
//! `POST /_freellama/v1/config/reload`; values pinned by the CLI or environment stay pinned, and
//! `GET /_freellama/v1/config` reports each value with the source that set it.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod adaptive;
mod breaker;
mod resolve;
mod settings;

pub(super) use adaptive::{
    AdaptiveLimiter, Completion, ExecutionProfile, ThroughputMetric, ThroughputMetricKind,
    note_limit_change, retry_after_seconds,
};
pub(super) use breaker::{BreakerProbe, Breakers};
use resolve::{process_env, resolve};
pub(super) use settings::RuntimeSettings;

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
    /// Admission units per task; embeddings charge this base for each four-input group.
    pub task_costs: Option<BTreeMap<super::TaskKind, NonZeroU32>>,
    pub context_mode: Option<ContextMode>,
    pub ollama_default_context: Option<u64>,
    pub auto_context_max: Option<u64>,
    pub usage_queue_capacity: Option<std::num::NonZeroUsize>,
    pub scopes: Option<super::ScopePolicy>,
    pub warming: Option<super::WarmingPolicy>,
    pub breaker_failures: Option<u32>,
    pub breaker_cooldown_seconds: Option<u64>,
}

impl RuntimeFile {
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or is not valid TOML for this schema.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let file: Self = toml::from_str(&text)?;
        if let Some(policy) = &file.scopes {
            policy.validate()?;
        }
        if let Some(policy) = &file.warming {
            policy.validate()?;
        }
        Ok(file)
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
    pub task_costs: BTreeMap<super::TaskKind, NonZeroU32>,
    pub context_mode: ContextMode,
    pub ollama_default_context: Option<u64>,
    pub auto_context_max: u64,
    /// Fixed at startup; changing it during reload is rejected.
    pub usage_queue_capacity: std::num::NonZeroUsize,
    pub scopes: super::ScopePolicy,
    pub warming: super::WarmingPolicy,
    pub breaker_failures: u32,
    pub breaker_cooldown_seconds: u64,
    /// Setting name -> `cli`, `env`, `env_invalid`, `file`, `default`, or `ollama_num_parallel`.
    /// `env_invalid` means the environment value did not parse and a lower source supplied it.
    #[serde(skip)]
    pub sources: BTreeMap<&'static str, &'static str>,
    /// One message per environment variable whose value did not parse and was ignored.
    #[serde(skip)]
    pub env_warnings: Vec<String>,
}

impl Tunables {
    pub(super) fn task_cost(&self, task: super::TaskKind) -> u32 {
        self.task_costs
            .get(&task)
            .map_or_else(|| default_task_cost(task).get(), |cost| cost.get())
    }

    pub(super) fn admission_costs(&self) -> Value {
        let embedding = self.task_cost(super::TaskKind::Embedding);
        json!({
            "base_by_task": self.task_costs,
            "embedding": if embedding == 1 {
                "ceil(input_items/4)".to_owned()
            } else {
                format!("{embedding} * ceil(input_items/4)")
            },
            "chat": self.task_cost(super::TaskKind::Completion),
            "vision": self.task_cost(super::TaskKind::Vision),
        })
    }

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

fn default_task_cost(task: super::TaskKind) -> NonZeroU32 {
    let cost = match task {
        super::TaskKind::Embedding => 1,
        super::TaskKind::Vision => 4,
        _ => 2,
    };
    NonZeroU32::new(cost).expect("default admission costs are positive")
}

#[cfg(test)]
mod tests;
