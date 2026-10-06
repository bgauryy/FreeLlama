//! Adaptive concurrency limiter driven by latency, throughput and memory pressure.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::admission::AdmissionPool;
use super::super::telemetry::Telemetry;

/// Additive-increase / multiplicative-decrease control of one backend's admission limit.
///
/// A bad completion (upstream failure, host memory pressure, or comparable throughput below 70% of
/// that comparable profile's healthy baseline) halves the limit, at most once per `DECREASE_SPACING`. Five
/// comparable positive-rate completions in a row raise it by one unit, up to the configured
/// ceiling. The fixed setting stays the ceiling, so this can only make `FreeLlama` more conservative.
#[derive(Clone, Default)]
pub(crate) struct AdaptiveLimiter {
    pub(crate) inner: Arc<StdMutex<AdaptiveState>>,
}

/// An incomplete, invalid, or cancelled forward cannot bridge healthy observations.
pub(crate) struct AdaptiveCompletionGuard {
    limiter: AdaptiveLimiter,
    pub(crate) completed: bool,
}

impl AdaptiveCompletionGuard {
    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for AdaptiveCompletionGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.limiter.reset_healthy_streak();
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct AdaptiveState {
    baseline_tps: BTreeMap<super::super::TaskKind, ProfileBaseline>,
    healthy_streak: u32,
    last_profile: Option<String>,
    last_decrease: Option<Instant>,
    last_reason: Option<String>,
}

/// Rate units are part of execution comparability, including persisted profile identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThroughputMetricKind {
    #[default]
    OutputTokensPerSecond,
    InputTokensPerSecond,
}

impl ThroughputMetricKind {
    pub(crate) const fn for_task(task: super::super::TaskKind) -> Self {
        if matches!(task, super::super::TaskKind::Embedding) {
            Self::InputTokensPerSecond
        } else {
            Self::OutputTokensPerSecond
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThroughputMetric {
    pub(crate) kind: ThroughputMetricKind,
    pub(crate) tokens_per_second: f64,
}

/// Prompt-free identity of one observed execution configuration and admission class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutionProfile {
    pub(crate) id: String,
    pub(crate) task: super::super::TaskKind,
    pub(crate) model: String,
    pub(crate) digest: String,
    pub(crate) context_tokens: u64,
    pub(crate) processor: String,
    #[serde(default)]
    pub(crate) metric_kind: ThroughputMetricKind,
}

#[derive(Debug)]
pub(crate) struct ProfileBaseline {
    pub(crate) profile: ExecutionProfile,
    pub(crate) rate: f64,
    pub(crate) samples: u32,
}

const DECREASE_SPACING: Duration = Duration::from_secs(10);

const HEALTHY_STREAK_FOR_INCREASE: u32 = 5;

const THROUGHPUT_DROP_RATIO: f64 = 0.7;

const BASELINE_MIN_SAMPLES: u32 = 3;

const BASELINE_ALPHA: f64 = 0.2;

/// One finished task as the controller sees it.
pub(crate) struct Completion<'a> {
    pub(crate) profile: Option<&'a ExecutionProfile>,
    pub(crate) failed: bool,
    pub(crate) memory_pressure: bool,
    pub(crate) metric: Option<ThroughputMetric>,
}

impl AdaptiveLimiter {
    pub(crate) fn completion_guard(&self) -> AdaptiveCompletionGuard {
        AdaptiveCompletionGuard {
            limiter: self.clone(),
            completed: false,
        }
    }

    pub(crate) fn reset_healthy_streak(&self) {
        let mut state = self.inner.lock().expect("adaptive state poisoned");
        state.healthy_streak = 0;
        state.last_profile = None;
    }
    /// Feed one completion; returns `Some((direction, new_limit))` when the limit changed.
    pub(crate) fn observe(
        &self,
        pool: &AdmissionPool,
        completion: &Completion<'_>,
    ) -> Option<(&'static str, usize)> {
        let mut state = self.inner.lock().expect("adaptive state poisoned");
        let slow = completion.metric.and_then(|metric| {
            let tps = metric.tokens_per_second;
            let profile = completion.profile?;
            let baseline = state.baseline_tps.get(&profile.task)?;
            (baseline.profile == *profile
                && metric.kind == profile.metric_kind
                && baseline.samples >= BASELINE_MIN_SAMPLES
                && tps.is_finite()
                && tps > 0.0
                && tps < baseline.rate * THROUGHPUT_DROP_RATIO)
                .then_some((tps, baseline.rate))
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
        let Some(profile) = completion.profile else {
            state.healthy_streak = 0;
            state.last_profile = None;
            return None;
        };
        let Some(metric) = completion.metric.filter(|metric| {
            metric.kind == profile.metric_kind
                && metric.tokens_per_second.is_finite()
                && metric.tokens_per_second > 0.0
        }) else {
            state.healthy_streak = 0;
            return None;
        };
        let tps = metric.tokens_per_second;
        if state.last_profile.as_deref() != Some(&profile.id) {
            state.healthy_streak = 0;
            state.last_profile = Some(profile.id.clone());
        }
        {
            let entry = state
                .baseline_tps
                .entry(profile.task)
                .or_insert_with(|| ProfileBaseline {
                    profile: profile.clone(),
                    rate: tps,
                    samples: 0,
                });
            if entry.profile != *profile {
                *entry = ProfileBaseline {
                    profile: profile.clone(),
                    rate: tps,
                    samples: 0,
                };
            }
            entry.rate = if entry.samples == 0 {
                tps
            } else {
                entry.rate * (1.0 - BASELINE_ALPHA) + tps * BASELINE_ALPHA
            };
            entry.samples = entry.samples.saturating_add(1);
        }
        state.healthy_streak = state.healthy_streak.saturating_add(1);
        if state.healthy_streak >= HEALTHY_STREAK_FOR_INCREASE && pool.total() < pool.ceiling() {
            state.healthy_streak = 0;
            let current = pool.total();
            let next = pool.set_limit(current + 1);
            return (next > current).then_some(("increase", next));
        }
        None
    }

    pub(crate) fn receipt(&self, pool: &AdmissionPool, enabled: bool) -> Value {
        let state = self.inner.lock().expect("adaptive state poisoned");
        json!({
            "enabled": enabled,
            "limit": pool.total(),
            "ceiling": pool.ceiling(),
            "healthy_streak": state.healthy_streak,
            "last_decrease_reason": state.last_reason,
            "profile_policy": "one_current_profile_per_task_serial_only",
            "current_profile_count": state.baseline_tps.len(),
            "profile_windows": state.baseline_tps.iter()
                .map(|(task, baseline)| (*task, json!({"metric_kind":baseline.profile.metric_kind,"rate":baseline.rate,"samples":baseline.samples,"profile":baseline.profile})))
                .collect::<BTreeMap<_, _>>(),
            "baseline_output_tokens_per_second_scope": "legacy_model_summary",
            "baseline_output_tokens_per_second": state.baseline_tps.values()
                .filter(|baseline| baseline.profile.metric_kind == ThroughputMetricKind::OutputTokensPerSecond)
                .map(|baseline| (baseline.profile.model.clone(), json!({"tps": baseline.rate, "samples": baseline.samples, "profile": baseline.profile})))
                .collect::<serde_json::Map<_, _>>(),
        })
    }
}

/// Estimated seconds until capacity frees up: waiting tasks ahead, times the recent average
/// service time, spread over the current limit. Clamped to 1..=120 for `Retry-After`.
pub(crate) fn retry_after_seconds(
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
pub(crate) fn note_limit_change(
    telemetry: &Telemetry,
    backend: &str,
    change: Option<(&str, usize)>,
) {
    if let Some((direction, limit)) = change {
        telemetry.record_limit_change(backend, direction);
        eprintln!("adaptive concurrency: {backend} limit {direction}d to {limit} units");
    }
}
