//! Persisted placement feedback: per-backend latency stats and the CPU/GPU placement they suggest.

use std::{collections::BTreeMap, io::Write, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;

use super::{Backend, ExecutionPreference, Objective, TaskKind, runtime};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct FeedbackStats {
    pub(crate) model: Option<String>,
    pub(crate) completed: u64,
    duration_samples: u64,
    total_work_unit_ns: u128,
    total_queue_wait_ms: u128,
    last_work_unit_ns: Option<u64>,
    #[serde(default)]
    pub(crate) profile: Option<runtime::ExecutionProfile>,
    #[serde(default)]
    profile_duration_samples: u64,
    #[serde(default)]
    profile_total_work_unit_ns: u128,
}

impl FeedbackStats {
    pub(crate) fn record(&mut self, model: &str, work_unit_ns: Option<u64>, queue_wait_ms: u128) {
        if self
            .model
            .as_deref()
            .is_some_and(|current| current != model)
        {
            *self = Self::default();
        }
        self.model = Some(model.to_owned());
        self.completed = self.completed.saturating_add(1);
        self.total_queue_wait_ms = self.total_queue_wait_ms.saturating_add(queue_wait_ms);
        if let Some(duration) = work_unit_ns {
            self.duration_samples = self.duration_samples.saturating_add(1);
            self.total_work_unit_ns = self.total_work_unit_ns.saturating_add(u128::from(duration));
            self.last_work_unit_ns = Some(duration);
        }
    }

    fn average_work_unit_ns(&self) -> Option<u128> {
        (self.duration_samples >= MIN_FEEDBACK_SAMPLES && self.total_work_unit_ns > 0)
            .then(|| self.total_work_unit_ns / u128::from(self.duration_samples))
    }

    pub(crate) fn record_profile(&mut self, profile: &runtime::ExecutionProfile, duration: u64) {
        if self.profile.as_ref() != Some(profile) {
            self.profile = Some(profile.clone());
            self.profile_duration_samples = 0;
            self.profile_total_work_unit_ns = 0;
        }
        self.profile_duration_samples = self.profile_duration_samples.saturating_add(1);
        self.profile_total_work_unit_ns = self
            .profile_total_work_unit_ns
            .saturating_add(u128::from(duration));
    }

    pub(crate) fn average_for_profile(&self, profile: &runtime::ExecutionProfile) -> Option<u128> {
        (self.profile.as_ref() == Some(profile)
            && self.profile_duration_samples >= MIN_FEEDBACK_SAMPLES
            && self.profile_total_work_unit_ns > 0)
            .then(|| self.profile_total_work_unit_ns / u128::from(self.profile_duration_samples))
    }

    pub(crate) fn receipt(&self) -> Value {
        json!({
            "model": self.model,
            "completed": self.completed,
            "duration_samples": self.duration_samples,
            "decision_ready": self.profile.as_ref().and_then(|profile| self.average_for_profile(profile)).is_some(),
            "learning_profile": self.profile,
            "profile_duration_samples": self.profile_duration_samples,
            "profile_policy": "one_current_profile_per_task_serial_only",
            "decision_metric": "nanoseconds_per_work_unit",
            "average_work_unit_ns": self.average_work_unit_ns(),
            "average_queue_wait_ms": (self.completed > 0)
                .then(|| self.total_queue_wait_ms / u128::from(self.completed)),
            "last_work_unit_ns": self.last_work_unit_ns,
        })
    }
}

pub(crate) const MIN_FEEDBACK_SAMPLES: u64 = 3;

pub(crate) const MIN_FEEDBACK_IMPROVEMENT_PERCENT: u128 = 10;

fn meaningfully_faster(candidate: u128, baseline: u128) -> bool {
    candidate.saturating_mul(100) < baseline.saturating_mul(100 - MIN_FEEDBACK_IMPROVEMENT_PERCENT)
}

/// The complete, request-local input to the placement hint calculation.
///
/// Keeping this decision independent from HTTP and semaphore ownership makes every combination
/// testable. The selected model is still resolved separately, so a hint can never bypass model
/// eligibility or an operator assignment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlacementSignals {
    pub(crate) route_is_pinned: bool,
    pub(crate) objective: Objective,
    pub(crate) execution_preference: ExecutionPreference,
    pub(crate) gpu_work_unit_ns: Option<u128>,
    pub(crate) cpu_work_unit_ns: Option<u128>,
    pub(crate) gpu_slots_available: usize,
    pub(crate) cpu_slots_available: usize,
    pub(crate) gpu_task_cost: usize,
    pub(crate) cpu_task_cost: usize,
    pub(crate) cpu_configured: bool,
}

pub(crate) fn desired_placement(signals: PlacementSignals) -> Option<(Backend, &'static str)> {
    let gpu_ready = signals.gpu_slots_available >= signals.gpu_task_cost;
    let cpu_ready = signals.cpu_configured && signals.cpu_slots_available >= signals.cpu_task_cost;
    match signals.execution_preference {
        ExecutionPreference::PreferCpu => {
            return Some(if cpu_ready || !gpu_ready {
                (Backend::Cpu, "preferred_backend_eligible")
            } else {
                (Backend::Gpu, "backend_capacity_available")
            });
        }
        ExecutionPreference::PreferGpu => {
            return Some(if gpu_ready || !cpu_ready {
                (Backend::Gpu, "preferred_backend_eligible")
            } else {
                (Backend::Cpu, "backend_capacity_available")
            });
        }
        ExecutionPreference::Auto => {}
    }
    if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
        return None;
    }
    match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
        (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
            return Some(if cpu_ready || !gpu_ready {
                (Backend::Cpu, "measured_backend_faster")
            } else {
                (Backend::Gpu, "backend_capacity_available")
            });
        }
        (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
            return Some(if gpu_ready || !cpu_ready {
                (Backend::Gpu, "measured_backend_faster")
            } else {
                (Backend::Cpu, "backend_capacity_available")
            });
        }
        _ => {}
    }
    if !gpu_ready && cpu_ready {
        return Some((Backend::Cpu, "backend_capacity_available"));
    }
    if !cpu_ready && gpu_ready {
        return Some((Backend::Gpu, "backend_capacity_available"));
    }
    None
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PlacementFeedback {
    pub(crate) gpu: BTreeMap<TaskKind, FeedbackStats>,
    pub(crate) cpu: BTreeMap<TaskKind, FeedbackStats>,
}

pub(crate) const FEEDBACK_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FeedbackSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) feedback: PlacementFeedback,
}

pub(crate) fn load_feedback(path: &Path) -> Result<PlacementFeedback> {
    if !path.exists() {
        return Ok(PlacementFeedback::default());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read feedback snapshot {}", path.display()))?;
    let snapshot: FeedbackSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse feedback snapshot {}", path.display()))?;
    ensure!(
        snapshot.schema_version == FEEDBACK_SCHEMA_VERSION,
        "unsupported feedback snapshot schema {} in {}",
        snapshot.schema_version,
        path.display()
    );
    Ok(snapshot.feedback)
}

pub(crate) fn persist_feedback(path: &Path, feedback: &PlacementFeedback) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create feedback directory {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create feedback temporary file in {}", parent.display()))?;
    serde_json::to_writer_pretty(
        temporary.as_file_mut(),
        &FeedbackSnapshot {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            feedback: feedback.clone(),
        },
    )
    .context("serialize feedback snapshot")?;
    temporary.as_file_mut().write_all(b"\n")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync feedback snapshot")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace feedback snapshot {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod feedback_tests {
    use super::super::{Backend, ExecutionPreference, Objective, TaskKind, feedback_work_unit_ns};
    use super::{FeedbackStats, PlacementSignals, desired_placement, meaningfully_faster};
    use serde_json::json;

    #[test]
    fn backend_feedback_requires_a_real_ten_percent_advantage() {
        assert!(!meaningfully_faster(90, 100));
        assert!(!meaningfully_faster(95, 100));
        assert!(meaningfully_faster(89, 100));
    }

    #[test]
    fn placement_decision_covers_every_signal_permutation() {
        let objectives = [Objective::Fastest, Objective::Balanced, Objective::Quality];
        let preferences = [
            ExecutionPreference::Auto,
            ExecutionPreference::PreferCpu,
            ExecutionPreference::PreferGpu,
        ];
        let gpu_scores = [None, Some(100)];
        let cpu_scores = [None, Some(89), Some(90), Some(100), Some(111), Some(112)];
        let mut checked = 0;

        for route_is_pinned in [false, true] {
            for objective in objectives {
                for execution_preference in preferences {
                    for gpu_work_unit_ns in gpu_scores {
                        for cpu_work_unit_ns in cpu_scores {
                            for gpu_slots_available in [0, 1, 4] {
                                for cpu_slots_available in [0, 1, 4] {
                                    for gpu_task_cost in [1, 2, 4] {
                                        for cpu_task_cost in [1, 2, 4] {
                                            for cpu_configured in [false, true] {
                                                let signals = PlacementSignals {
                                                    route_is_pinned,
                                                    objective,
                                                    execution_preference,
                                                    gpu_work_unit_ns,
                                                    cpu_work_unit_ns,
                                                    gpu_slots_available,
                                                    cpu_slots_available,
                                                    gpu_task_cost,
                                                    cpu_task_cost,
                                                    cpu_configured,
                                                };
                                                let expected = placement_oracle(signals);
                                                assert_eq!(
                                                    desired_placement(signals),
                                                    expected,
                                                    "placement mismatch for {signals:?}"
                                                );
                                                checked += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        assert_eq!(checked, 2 * 3 * 3 * 2 * 6 * 3 * 3 * 3 * 3 * 2);
    }

    fn placement_oracle(signals: PlacementSignals) -> Option<(Backend, &'static str)> {
        let gpu_ready = signals.gpu_slots_available >= signals.gpu_task_cost;
        let cpu_ready =
            signals.cpu_configured && signals.cpu_slots_available >= signals.cpu_task_cost;
        match signals.execution_preference {
            ExecutionPreference::PreferCpu => {
                return Some(if cpu_ready || !gpu_ready {
                    (Backend::Cpu, "preferred_backend_eligible")
                } else {
                    (Backend::Gpu, "backend_capacity_available")
                });
            }
            ExecutionPreference::PreferGpu => {
                return Some(if gpu_ready || !cpu_ready {
                    (Backend::Gpu, "preferred_backend_eligible")
                } else {
                    (Backend::Cpu, "backend_capacity_available")
                });
            }
            ExecutionPreference::Auto => {}
        }
        if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
            return None;
        }
        match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
            (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
                return Some(if cpu_ready || !gpu_ready {
                    (Backend::Cpu, "measured_backend_faster")
                } else {
                    (Backend::Gpu, "backend_capacity_available")
                });
            }
            (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
                return Some(if gpu_ready || !cpu_ready {
                    (Backend::Gpu, "measured_backend_faster")
                } else {
                    (Backend::Cpu, "backend_capacity_available")
                });
            }
            _ => {}
        }
        if !gpu_ready && cpu_ready {
            return Some((Backend::Cpu, "backend_capacity_available"));
        }
        if !cpu_ready && gpu_ready {
            return Some((Backend::Gpu, "backend_capacity_available"));
        }
        None
    }

    #[test]
    fn feedback_readiness_covers_sample_and_duration_permutations() {
        let mut checked = 0;
        for duration_samples in 0..=4 {
            for total_work_unit_ns in [0, 100] {
                let stats = FeedbackStats {
                    duration_samples,
                    total_work_unit_ns,
                    ..FeedbackStats::default()
                };
                let expected = (duration_samples >= 3 && total_work_unit_ns > 0)
                    .then_some(total_work_unit_ns / u128::from(duration_samples.max(1)));
                assert_eq!(stats.average_work_unit_ns(), expected);
                checked += 1;
            }
        }
        assert_eq!(checked, 10);
    }

    #[test]
    fn feedback_never_combines_different_models_in_one_backend_bucket() {
        let mut stats = FeedbackStats::default();
        for _ in 0..3 {
            stats.record("model-a", Some(100), 4);
        }
        assert_eq!(stats.average_work_unit_ns(), Some(100));
        assert_eq!(stats.profile, None);

        stats.record("model-b", Some(50), 2);
        assert_eq!(stats.model.as_deref(), Some("model-b"));
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.duration_samples, 1);
        assert_eq!(stats.profile, None);
    }

    #[test]
    fn legacy_feedback_is_retained_as_diagnostics_without_becoming_profile_evidence() {
        let legacy = json!({"model":"model-a","completed":7,"duration_samples":5,
            "total_work_unit_ns":500,"total_queue_wait_ms":14,"last_work_unit_ns":100});
        let mut stats: FeedbackStats = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(stats.average_work_unit_ns(), Some(100));
        assert_eq!(stats.receipt()["decision_ready"], false);
        let profile = super::runtime::ExecutionProfile {
            id: "profile-a".into(),
            task: TaskKind::Completion,
            model: "model-a".into(),
            digest: "revision-a".into(),
            context_tokens: 4096,
            processor: "gpu".into(),
            metric_kind: super::runtime::ThroughputMetricKind::OutputTokensPerSecond,
        };
        for _ in 0..3 {
            stats.record_profile(&profile, 50);
        }
        assert_eq!(stats.average_for_profile(&profile), Some(50));
        let stored = serde_json::to_value(&stats).unwrap();
        for field in legacy.as_object().unwrap().keys() {
            assert_eq!(
                stored[field], legacy[field],
                "legacy aggregate must survive additive profile learning"
            );
        }
        let restarted: FeedbackStats = serde_json::from_value(stored).unwrap();
        assert_eq!(restarted.average_for_profile(&profile), Some(50));
        let mut changed = profile.clone();
        changed.id = "profile-b".into();
        stats.record_profile(&changed, 10);
        assert_eq!(stats.average_for_profile(&profile), None);
        assert_eq!(stats.average_for_profile(&changed), None);
        assert_eq!(stats.profile_duration_samples, 1);
    }

    #[test]
    fn feedback_metric_uses_the_right_ollama_counters_for_every_task() {
        let response = json!({
            "total_duration": 1_200,
            "prompt_eval_count": 12,
            "eval_duration": 700,
            "eval_count": 7,
        });
        let tasks = [
            TaskKind::Completion,
            TaskKind::Coding,
            TaskKind::CodeRepair,
            TaskKind::Tools,
            TaskKind::Browser,
            TaskKind::Vision,
            TaskKind::Embedding,
            TaskKind::LongContext,
        ];
        for task in tasks {
            assert_eq!(
                feedback_work_unit_ns(task, &response),
                Some(100),
                "wrong normalization counters for {task:?}"
            );
        }
    }
}
