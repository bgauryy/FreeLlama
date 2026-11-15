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
mod tests;
