//! Persisted placement feedback: per-backend latency stats and the CPU/GPU placement they suggest.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{Backend, ExecutionPreference, Objective, TaskKind, runtime};
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Limits apply independently to each task/backend, so many models cannot grow the cache forever.
const MAX_PROFILE_WINDOWS: usize = 32;
const MAX_PROFILE_SAMPLES: usize = 32;

/// Prompt-free phases from a qualified execution. `upstream_setup_ns` is Ollama's reported setup
/// time, not proof of a physical disk load. Unknown phases stay absent rather than becoming zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct PhaseSample {
    pub(crate) queue_wait_ns: Option<u64>,
    pub(crate) upstream_setup_ns: Option<u64>,
    pub(crate) prefill_ns_per_token: Option<u64>,
    pub(crate) decode_ns_per_token: Option<u64>,
}

/// Each phase qualifies independently after three observations under exactly the requested profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PhaseEstimate {
    pub(crate) queue_wait_ns: Option<u128>,
    pub(crate) upstream_setup_ns: Option<u128>,
    pub(crate) prefill_ns_per_token: Option<u128>,
    pub(crate) decode_ns_per_token: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ProfileSample {
    observed_at_ns: u64,
    work_unit_ns: u64,
    phases: PhaseSample,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileWindow {
    profile: runtime::ExecutionProfile,
    /// Unique observation ids make merging repeated or divergent process snapshots idempotent.
    samples: BTreeMap<String, ProfileSample>,
}

impl ProfileWindow {
    fn newest(&self) -> u64 {
        self.samples
            .values()
            .map(|sample| sample.observed_at_ns)
            .max()
            .unwrap_or(0)
    }

    fn prune(&mut self) {
        self.samples.retain(|_, sample| sample.work_unit_ns > 0);
        while self.samples.len() > MAX_PROFILE_SAMPLES {
            let oldest = self
                .samples
                .iter()
                .min_by_key(|(id, sample)| (sample.observed_at_ns, *id))
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                self.samples.remove(&oldest);
            }
        }
    }

    fn average(&self, phase: impl Fn(&ProfileSample) -> Option<u64>) -> Option<u128> {
        let (sum, count) = self
            .samples
            .values()
            .filter_map(phase)
            .fold((0_u128, 0_u64), |(sum, count), value| {
                (sum.saturating_add(u128::from(value)), count + 1)
            });
        (count >= MIN_FEEDBACK_SAMPLES).then(|| sum / u128::from(count))
    }
}

fn profile_key(profile: &runtime::ExecutionProfile) -> String {
    // `id` already hashes the effective runtime controls. Include every exposed identity field
    // as well, so neither a changed digest nor a mismatched deserialized profile can borrow it.
    let bytes = serde_json::to_vec(profile).expect("execution profile serializes");
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn observed_at_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct FeedbackStats {
    pub(crate) model: Option<String>,
    pub(crate) completed: u64,
    duration_samples: u64,
    total_work_unit_ns: u128,
    total_queue_wait_ms: u128,
    last_work_unit_ns: Option<u64>,
    pub(crate) profile: Option<runtime::ExecutionProfile>,
    profile_duration_samples: u64,
    profiles: BTreeMap<String, ProfileWindow>,
    last_observed_ns: u64,
}

impl FeedbackStats {
    pub(crate) fn record(&mut self, model: &str, work_unit_ns: Option<u64>, queue_wait_ms: u128) {
        if self
            .model
            .as_deref()
            .is_some_and(|current| current != model)
        {
            let profiles = std::mem::take(&mut self.profiles);
            *self = Self {
                profiles,
                ..Self::default()
            };
        }
        self.last_observed_ns = observed_at_ns();
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

    #[cfg(test)]
    fn record_profile(&mut self, profile: &runtime::ExecutionProfile, duration: u64) {
        self.record_profile_with_phases(profile, duration, PhaseSample::default());
    }

    pub(crate) fn record_profile_with_phases(
        &mut self,
        profile: &runtime::ExecutionProfile,
        duration: u64,
        phases: PhaseSample,
    ) {
        if duration == 0 {
            return;
        }
        let at = observed_at_ns();
        let window = self
            .profiles
            .entry(profile_key(profile))
            .or_insert_with(|| ProfileWindow {
                profile: profile.clone(),
                samples: BTreeMap::new(),
            });
        window.samples.insert(
            uuid::Uuid::new_v4().to_string(),
            ProfileSample {
                observed_at_ns: at,
                work_unit_ns: duration,
                phases,
            },
        );
        window.prune();
        self.profile = Some(profile.clone());
        self.last_observed_ns = at;
        self.prune_profiles();
        self.refresh_profile_diagnostics();
    }

    pub(crate) fn average_for_profile(&self, profile: &runtime::ExecutionProfile) -> Option<u128> {
        self.profile_window(profile)?
            .average(|sample| Some(sample.work_unit_ns))
    }

    pub(crate) fn phases_for_profile(
        &self,
        profile: &runtime::ExecutionProfile,
    ) -> Option<PhaseEstimate> {
        let window = self.profile_window(profile)?;
        Some(PhaseEstimate {
            queue_wait_ns: window.average(|sample| sample.phases.queue_wait_ns),
            upstream_setup_ns: window.average(|sample| sample.phases.upstream_setup_ns),
            prefill_ns_per_token: window
                .average(|sample| sample.phases.prefill_ns_per_token.filter(|rate| *rate > 0)),
            decode_ns_per_token: window
                .average(|sample| sample.phases.decode_ns_per_token.filter(|rate| *rate > 0)),
        })
    }

    fn profile_window(&self, profile: &runtime::ExecutionProfile) -> Option<&ProfileWindow> {
        self.profiles
            .get(&profile_key(profile))
            .filter(|window| &window.profile == profile)
    }

    fn prune_profiles(&mut self) {
        self.profiles.retain(|key, window| {
            window.prune();
            *key == profile_key(&window.profile) && !window.samples.is_empty()
        });
        while self.profiles.len() > MAX_PROFILE_WINDOWS {
            let oldest = self
                .profiles
                .iter()
                .min_by_key(|(key, window)| (window.newest(), *key))
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.profiles.remove(&oldest);
            }
        }
    }

    fn refresh_profile_diagnostics(&mut self) {
        if let Some(window) = self
            .profile
            .as_ref()
            .and_then(|profile| self.profile_window(profile))
        {
            self.profile_duration_samples = window.samples.len() as u64;
        } else {
            self.profile = None;
            self.profile_duration_samples = 0;
        }
    }

    pub(crate) fn receipt(&self) -> Value {
        json!({
            "model": self.model,
            "completed": self.completed,
            "duration_samples": self.duration_samples,
            "decision_ready": self.profile.as_ref().and_then(|profile| self.average_for_profile(profile)).is_some(),
            "learning_profile": self.profile,
            "profile_duration_samples": self.profile_duration_samples,
            "profile_policy": "bounded_exact_profiles_serial_only",
            "retained_profiles": self.profiles.len(),
            "profile_limit": MAX_PROFILE_WINDOWS,
            "samples_per_profile_limit": MAX_PROFILE_SAMPLES,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent request signal; the permutation test covers every combination"
)]
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
    /// Whether each backend's eligible model is already loaded.
    pub(crate) gpu_resident: bool,
    pub(crate) cpu_resident: bool,
}

/// A measured speed advantage does not justify a cold load while the other backend already holds
/// an eligible model with a free slot: the load is certain, the saving is not, and following speed
/// alone can move runners back and forth between backends. Warming the faster model switches.
fn measured_choice(
    faster: Backend,
    (faster_ready, faster_resident): (bool, bool),
    (other_ready, other_resident): (bool, bool),
) -> (Backend, &'static str) {
    let other = match faster {
        Backend::Gpu => Backend::Cpu,
        Backend::Cpu => Backend::Gpu,
    };
    if !faster_resident && other_resident && other_ready {
        (other, "resident_backend_avoids_cold_load")
    } else if faster_ready || !other_ready {
        (faster, "measured_backend_faster")
    } else {
        (other, "backend_capacity_available")
    }
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
    let gpu = (gpu_ready, signals.gpu_resident);
    let cpu = (cpu_ready, signals.cpu_resident);
    match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
        (Some(gpu_ns), Some(cpu_ns)) if meaningfully_faster(cpu_ns, gpu_ns) => {
            return Some(measured_choice(Backend::Cpu, cpu, gpu));
        }
        (Some(gpu_ns), Some(cpu_ns)) if meaningfully_faster(gpu_ns, cpu_ns) => {
            return Some(measured_choice(Backend::Gpu, gpu, cpu));
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

pub(crate) const FEEDBACK_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FeedbackSnapshot {
    pub(crate) schema_version: u32,
    pub(crate) feedback: PlacementFeedback,
}

/// The name of the placement-feedback snapshot in the shared database.
const FEEDBACK_SNAPSHOT: &str = "placement_feedback";

/// Where the original schema-1 body is kept once it has been migrated, so nothing is lost.
const FEEDBACK_V1_BACKUP: &str = "placement_feedback_v1_backup";

pub(crate) fn load_feedback(store: &Store) -> Result<PlacementFeedback> {
    let Some(body) = store.snapshot(FEEDBACK_SNAPSHOT)? else {
        return Ok(PlacementFeedback::default());
    };
    let (feedback, migrated) = parse_snapshot(&body)?;
    if migrated {
        backup_v1(store, &body)?;
        eprintln!(
            "placement feedback migrated from schema 1 to {FEEDBACK_SCHEMA_VERSION}; aggregate \
             counters kept as diagnostics, original saved as snapshot `{FEEDBACK_V1_BACKUP}`"
        );
    }
    Ok(feedback)
}

/// Startup never aborts on stored feedback it cannot read: the process learns from empty history,
/// and the returned warning says which snapshot was skipped. Persistence still refuses to replace
/// that evidence, so a later build that understands it can use it.
pub(crate) fn load_feedback_or_empty(store: &Store) -> (PlacementFeedback, Option<String>) {
    match load_feedback(store) {
        Ok(feedback) => (feedback, None),
        Err(error) => {
            let warning = format!(
                "placement feedback snapshot `{FEEDBACK_SNAPSHOT}` was not loaded ({error:#}); \
                 starting with empty feedback and leaving the stored snapshot untouched"
            );
            eprintln!("{warning}");
            (PlacementFeedback::default(), Some(warning))
        }
    }
}

fn backup_v1(store: &Store, body: &str) -> Result<()> {
    if store.snapshot(FEEDBACK_V1_BACKUP)?.is_none() {
        store.put_snapshot(FEEDBACK_V1_BACKUP, body)?;
    }
    Ok(())
}

/// Parse a stored body; the flag is true when it was schema 1 and was migrated in memory.
fn parse_snapshot(body: &str) -> Result<(PlacementFeedback, bool)> {
    let mut raw: Value =
        serde_json::from_str(body).context("parse the stored feedback snapshot")?;
    let version = raw["schema_version"].as_u64();
    let migrated = version == Some(1);
    if migrated {
        migrate_v1(&mut raw);
    } else {
        ensure!(
            version == Some(u64::from(FEEDBACK_SCHEMA_VERSION)),
            "unsupported feedback snapshot schema {}; this build reads schemas 1 and \
             {FEEDBACK_SCHEMA_VERSION}",
            version.map_or_else(|| "(missing)".to_owned(), |version| version.to_string())
        );
    }
    let mut snapshot: FeedbackSnapshot =
        serde_json::from_value(raw).context("parse the stored feedback snapshot")?;
    for stats in snapshot
        .feedback
        .gpu
        .values_mut()
        .chain(snapshot.feedback.cpu.values_mut())
    {
        stats.prune_profiles();
        stats.refresh_profile_diagnostics();
    }
    Ok((snapshot.feedback, migrated))
}

/// Schema 1 kept only aggregate counters and one running profile average. Keep the counters as
/// diagnostics; the profile average was never per-sample evidence, so v2 evidence starts empty.
fn migrate_v1(raw: &mut Value) {
    raw["schema_version"] = json!(FEEDBACK_SCHEMA_VERSION);
    for backend in ["gpu", "cpu"] {
        let Some(tasks) = raw["feedback"][backend].as_object_mut() else {
            continue;
        };
        for stats in tasks.values_mut().filter_map(Value::as_object_mut) {
            stats.remove("profile_total_work_unit_ns");
            stats.insert("profile".into(), Value::Null);
            stats.insert("profile_duration_samples".into(), json!(0));
            stats.insert("profiles".into(), json!({}));
            stats.insert("last_observed_ns".into(), json!(0));
        }
    }
}

/// Merge independent profile observations, rather than letting one model's longer history replace
/// every other model. Sample ids make repeated writes and forked process snapshots idempotent.
pub(crate) fn persist_feedback(store: &Store, feedback: &PlacementFeedback) -> Result<()> {
    if let Some(body) = store.snapshot(FEEDBACK_SNAPSHOT)?
        && parse_snapshot(&body).is_ok_and(|(_, migrated)| migrated)
    {
        backup_v1(store, &body)?;
    }
    store.update_snapshot(FEEDBACK_SNAPSHOT, |current| {
        let stored = current
            .map(|body| parse_snapshot(body).map(|(feedback, _)| feedback))
            .transpose()?
            .unwrap_or_default();
        serde_json::to_string(&FeedbackSnapshot {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            feedback: merged(stored, feedback),
        })
        .context("serialize feedback snapshot")
    })
}

fn merged(mut stored: PlacementFeedback, ours: &PlacementFeedback) -> PlacementFeedback {
    for (theirs, mine) in [(&mut stored.gpu, &ours.gpu), (&mut stored.cpu, &ours.cpu)] {
        for (task, stats) in mine {
            let joined = merge_stats(theirs.get(task).unwrap_or(&FeedbackStats::default()), stats);
            theirs.insert(*task, joined);
        }
    }
    stored
}

fn merge_stats(stored: &FeedbackStats, ours: &FeedbackStats) -> FeedbackStats {
    // Freshness wins over an old model's large completion count. The rest is only a
    // deterministic tie-break for equal times.
    let rank = |stats: &FeedbackStats| {
        (
            stats.last_observed_ns,
            stats.completed,
            stats.duration_samples,
            stats.total_work_unit_ns,
            stats.total_queue_wait_ms,
            stats.last_work_unit_ns,
            stats.model.clone(),
            stats.profile.as_ref().map(|profile| profile.id.clone()),
        )
    };
    let mut result = if rank(ours) > rank(stored) {
        ours.clone()
    } else {
        stored.clone()
    };
    result.profiles.clone_from(&stored.profiles);
    for (key, incoming) in &ours.profiles {
        let window = result
            .profiles
            .entry(key.clone())
            .or_insert_with(|| ProfileWindow {
                profile: incoming.profile.clone(),
                samples: BTreeMap::new(),
            });
        for (id, sample) in &incoming.samples {
            let existing = window
                .samples
                .entry(id.clone())
                .or_insert_with(|| sample.clone());
            // Repeated snapshots contain identical samples. A conflicting id is resolved in
            // the same conservative order whichever process saved first; it never counts twice.
            if *sample > *existing {
                existing.clone_from(sample);
            }
        }
    }
    result.prune_profiles();
    result.refresh_profile_diagnostics();
    result
}

#[cfg(test)]
mod feedback_tests {
    use super::super::{Backend, ExecutionPreference, Objective, TaskKind, feedback_work_unit_ns};
    use super::{FeedbackStats, PlacementSignals, desired_placement, meaningfully_faster};
    use serde_json::json;

    #[test]
    fn two_processes_saving_feedback_keep_the_longer_history_per_task() {
        let store = crate::store::Store::in_memory().unwrap();
        let stats = |completed| FeedbackStats {
            completed,
            ..FeedbackStats::default()
        };
        let mut first = super::PlacementFeedback::default();
        first.gpu.insert(TaskKind::Completion, stats(9));
        super::persist_feedback(&store, &first).unwrap();
        // A second process that started earlier knows less about completion, more about embedding.
        let mut second = super::PlacementFeedback::default();
        second.gpu.insert(TaskKind::Completion, stats(2));
        second.gpu.insert(TaskKind::Embedding, stats(4));
        super::persist_feedback(&store, &second).unwrap();
        let saved = super::load_feedback(&store).unwrap();
        assert_eq!(saved.gpu[&TaskKind::Completion].completed, 9);
        assert_eq!(saved.gpu[&TaskKind::Embedding].completed, 4);
    }

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
                                                for (gpu_resident, cpu_resident) in RESIDENCY {
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
                                                        gpu_resident,
                                                        cpu_resident,
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
        }

        assert_eq!(checked, 2 * 3 * 3 * 2 * 6 * 3 * 3 * 3 * 3 * 2 * 4);
    }

    const RESIDENCY: [(bool, bool); 4] =
        [(false, false), (false, true), (true, false), (true, true)];

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
                return Some(
                    if !signals.cpu_resident && signals.gpu_resident && gpu_ready {
                        (Backend::Gpu, "resident_backend_avoids_cold_load")
                    } else if cpu_ready || !gpu_ready {
                        (Backend::Cpu, "measured_backend_faster")
                    } else {
                        (Backend::Gpu, "backend_capacity_available")
                    },
                );
            }
            (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
                return Some(
                    if !signals.gpu_resident && signals.cpu_resident && cpu_ready {
                        (Backend::Cpu, "resident_backend_avoids_cold_load")
                    } else if gpu_ready || !cpu_ready {
                        (Backend::Gpu, "measured_backend_faster")
                    } else {
                        (Backend::Cpu, "backend_capacity_available")
                    },
                );
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
    fn aggregate_diagnostics_never_become_profile_evidence() {
        let mut stats = FeedbackStats::default();
        for _ in 0..5 {
            stats.record("model-a", Some(100), 2);
        }
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
        assert_eq!(stored["completed"], 5);
        assert_eq!(stored["total_work_unit_ns"], 500);
        let restarted: FeedbackStats = serde_json::from_value(stored).unwrap();
        assert_eq!(restarted.average_for_profile(&profile), Some(50));
        let mut changed = profile.clone();
        changed.id = "profile-b".into();
        stats.record_profile(&changed, 10);
        assert_eq!(stats.average_for_profile(&profile), Some(50));
        assert_eq!(stats.average_for_profile(&changed), None);
        assert_eq!(stats.profile_duration_samples, 1);
    }

    fn profile(name: &str) -> super::runtime::ExecutionProfile {
        super::runtime::ExecutionProfile {
            id: format!("profile-{name}"),
            task: TaskKind::Completion,
            model: name.into(),
            digest: format!("digest-{name}"),
            context_tokens: 4096,
            processor: "gpu".into(),
            metric_kind: super::runtime::ThroughputMetricKind::OutputTokensPerSecond,
        }
    }

    fn learn(stats: &mut FeedbackStats, profile: &super::runtime::ExecutionProfile, rate: u64) {
        stats.record(&profile.model, Some(rate), 0);
        stats.record_profile(profile, rate);
    }

    #[test]
    fn alternating_models_keep_separate_exact_profile_evidence() {
        let (a, b) = (profile("a"), profile("b"));
        let mut stats = FeedbackStats::default();
        for _ in 0..3 {
            learn(&mut stats, &a, 100);
            learn(&mut stats, &b, 200);
        }
        assert_eq!(stats.average_for_profile(&a), Some(100));
        assert_eq!(stats.average_for_profile(&b), Some(200));
        // Even an accidentally reused opaque id cannot transfer evidence across a revision,
        // context, processor, model, task, or metric kind.
        for field in [
            "digest",
            "context",
            "processor",
            "model",
            "task",
            "metric",
            "id",
        ] {
            let mut changed = a.clone();
            match field {
                "digest" => changed.digest = "replacement".into(),
                "context" => changed.context_tokens = 8192,
                "processor" => changed.processor = "cpu".into(),
                "model" => changed.model = "other".into(),
                "task" => changed.task = TaskKind::Coding,
                "metric" => {
                    changed.metric_kind =
                        super::runtime::ThroughputMetricKind::InputTokensPerSecond;
                }
                _ => changed.id = "other-runtime-controls".into(),
            }
            assert_eq!(stats.average_for_profile(&changed), None, "{field}");
        }
    }

    #[test]
    fn restarted_history_unions_process_samples_without_double_counting() {
        let store = crate::store::Store::in_memory().unwrap();
        let (a, b) = (profile("a"), profile("b"));
        let mut initial = super::PlacementFeedback::default();
        for _ in 0..12 {
            learn(
                initial.gpu.entry(TaskKind::Completion).or_default(),
                &a,
                100,
            );
        }
        super::persist_feedback(&store, &initial).unwrap();
        let mut first = super::load_feedback(&store).unwrap();
        let mut second = first.clone();
        learn(first.gpu.get_mut(&TaskKind::Completion).unwrap(), &a, 200);
        for _ in 0..3 {
            learn(second.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 50);
        }
        // A newer model has fewer diagnostic completions, but retains its evidence on restart.
        super::persist_feedback(&store, &second).unwrap();
        super::persist_feedback(&store, &first).unwrap();
        super::persist_feedback(&store, &first).unwrap();
        let saved = super::load_feedback(&store).unwrap();
        let stats = &saved.gpu[&TaskKind::Completion];
        assert_eq!(stats.average_for_profile(&a), Some(1400 / 13));
        assert_eq!(stats.average_for_profile(&b), Some(50));
        assert_eq!(stats.profile_window(&a).unwrap().samples.len(), 13);
        assert_eq!(stats.profile_window(&b).unwrap().samples.len(), 3);
        assert_eq!(stats.model.as_deref(), Some("b"));
        assert_eq!(stats.completed, 3);

        // Two processes branching from the same saved history contribute distinct observations.
        let mut left = saved.clone();
        let mut right = saved;
        learn(left.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 100);
        learn(right.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 200);
        super::persist_feedback(&store, &left).unwrap();
        super::persist_feedback(&store, &right).unwrap();
        let final_state = super::load_feedback(&store).unwrap();
        let stats = &final_state.gpu[&TaskKind::Completion];
        assert_eq!(stats.profile_window(&b).unwrap().samples.len(), 5);
        assert_eq!(stats.average_for_profile(&b), Some(90));
    }

    fn v1_body() -> String {
        json!({
            "schema_version":1,
            "feedback":{"gpu":{"completion":{
                "model":"a","completed":100,"duration_samples":100,
                "total_work_unit_ns":10000,"total_queue_wait_ms":7,"last_work_unit_ns":100,
                "profile":profile("a"),"profile_duration_samples":100,
                "profile_total_work_unit_ns":10000
            }},"cpu":{}}
        })
        .to_string()
    }

    #[test]
    fn a_v1_snapshot_migrates_keeps_aggregates_and_is_backed_up() {
        let store = crate::store::Store::in_memory().unwrap();
        let old = v1_body();
        store.put_snapshot(super::FEEDBACK_SNAPSHOT, &old).unwrap();
        let loaded = super::load_feedback(&store).unwrap();
        let stats = &loaded.gpu[&TaskKind::Completion];
        assert_eq!(stats.completed, 100);
        assert_eq!(stats.duration_samples, 100);
        assert_eq!(stats.total_work_unit_ns, 10000);
        assert_eq!(stats.total_queue_wait_ms, 7);
        // Diagnostics only: no profile evidence is invented from the v1 running average.
        assert!(stats.profile.is_none());
        assert_eq!(stats.average_for_profile(&profile("a")), None);
        assert_eq!(
            store
                .snapshot(super::FEEDBACK_V1_BACKUP)
                .unwrap()
                .as_deref(),
            Some(old.as_str())
        );
        // Until something is learned the stored body stays schema 1; the next persist writes v2
        // and the backup keeps the original.
        super::persist_feedback(&store, &loaded).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().unwrap())
                .unwrap();
        assert_eq!(written["schema_version"], 2);
        assert_eq!(written["feedback"]["gpu"]["completion"]["completed"], 100);
        assert_eq!(
            store
                .snapshot(super::FEEDBACK_V1_BACKUP)
                .unwrap()
                .as_deref(),
            Some(old.as_str())
        );
        assert_eq!(super::load_feedback(&store).unwrap().gpu.len(), 1);
    }

    #[test]
    fn unknown_or_corrupt_feedback_starts_empty_and_is_not_overwritten() {
        for body in [
            "not-json",
            r#"{"schema_version":999,"feedback":{"gpu":{},"cpu":{}}}"#,
        ] {
            let store = crate::store::Store::in_memory().unwrap();
            store.put_snapshot(super::FEEDBACK_SNAPSHOT, body).unwrap();
            let (feedback, warning) = super::load_feedback_or_empty(&store);
            assert!(feedback.gpu.is_empty() && feedback.cpu.is_empty());
            let warning = warning.expect("a warning names the skipped snapshot");
            assert!(warning.contains("placement_feedback"), "{warning}");
            assert!(super::persist_feedback(&store, &feedback).is_err());
            assert_eq!(
                store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().as_deref(),
                Some(body)
            );
        }
    }

    #[test]
    fn phase_estimates_require_independent_valid_observations() {
        let a = profile("a");
        let mut stats = FeedbackStats::default();
        for index in 0..3 {
            stats.record_profile_with_phases(
                &a,
                200,
                super::PhaseSample {
                    queue_wait_ns: Some(0),
                    upstream_setup_ns: (index < 2).then_some(10),
                    prefill_ns_per_token: Some(100),
                    decode_ns_per_token: Some(200),
                },
            );
        }
        assert_eq!(
            stats.phases_for_profile(&a),
            Some(super::PhaseEstimate {
                queue_wait_ns: Some(0),
                upstream_setup_ns: None,
                prefill_ns_per_token: Some(100),
                decode_ns_per_token: Some(200),
            })
        );
        stats.record_profile_with_phases(
            &a,
            200,
            super::PhaseSample {
                upstream_setup_ns: Some(10),
                prefill_ns_per_token: Some(0),
                ..super::PhaseSample::default()
            },
        );
        let estimate = stats.phases_for_profile(&a).unwrap();
        assert_eq!(estimate.upstream_setup_ns, Some(10));
        assert_eq!(estimate.prefill_ns_per_token, Some(100));
        assert_eq!(stats.phases_for_profile(&profile("unknown")), None);
    }

    #[test]
    fn history_bounds_keep_the_newest_samples_and_profiles() {
        let a = profile("a");
        let mut stats = FeedbackStats::default();
        for index in 1..=40 {
            stats.record_profile(&a, index);
            let window = stats.profiles.get_mut(&super::profile_key(&a)).unwrap();
            let sample = window
                .samples
                .values_mut()
                .max_by_key(|sample| sample.observed_at_ns)
                .unwrap();
            sample.observed_at_ns = index;
        }
        assert_eq!(stats.profile_window(&a).unwrap().samples.len(), 32);
        assert_eq!(stats.average_for_profile(&a), Some(24)); // mean of 9..=40
        for index in 1..=40 {
            let p = profile(&format!("model-{index:02}"));
            stats.record_profile(&p, 100);
            for sample in stats
                .profiles
                .get_mut(&super::profile_key(&p))
                .unwrap()
                .samples
                .values_mut()
            {
                sample.observed_at_ns = 100 + index;
            }
        }
        assert_eq!(stats.profiles.len(), super::MAX_PROFILE_WINDOWS);
        assert!(stats.profile_window(&a).is_none());
        assert!(stats.profile_window(&profile("model-08")).is_none());
        assert!(stats.profile_window(&profile("model-09")).is_some());
        assert!(stats.profile_window(&profile("model-40")).is_some());
    }

    #[test]
    fn snapshot_merge_is_order_independent_even_for_conflicting_sample_ids() {
        let a = profile("a");
        let mut first = FeedbackStats::default();
        learn(&mut first, &a, 100);
        let mut second = first.clone();
        let window = second.profiles.get_mut(&super::profile_key(&a)).unwrap();
        window.samples.values_mut().next().unwrap().work_unit_ns = 200;
        let forward = super::merge_stats(&first, &second);
        let reverse = super::merge_stats(&second, &first);
        assert_eq!(
            serde_json::to_value(&forward).unwrap(),
            serde_json::to_value(&reverse).unwrap()
        );
        let samples = &forward.profile_window(&a).unwrap().samples;
        assert_eq!(samples.len(), 1);
        assert_eq!(samples.values().next().unwrap().work_unit_ns, 200);
    }

    #[test]
    fn persistence_does_not_replace_corrupt_or_unknown_version_evidence() {
        let store = crate::store::Store::in_memory().unwrap();
        for body in [
            "not-json",
            r#"{"schema_version":999,"feedback":{"gpu":{},"cpu":{}}}"#,
        ] {
            store.put_snapshot(super::FEEDBACK_SNAPSHOT, body).unwrap();
            assert!(super::persist_feedback(&store, &super::PlacementFeedback::default()).is_err());
            assert_eq!(
                store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().as_deref(),
                Some(body)
            );
        }
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
