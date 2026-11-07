//! Which resident model to unload when a load needs room.
//!
//! Ollama manages its own runner expiry and eviction using engine state. `FreeLlama` additionally
//! knows which models have tasks queued or running, how often each one is used, and observed
//! request-to-runner-ready costs. The planner uses that to pick the cheapest set of
//! runners to unload, in the spirit of llama-swap's `evict_costs` and `LocalAI`'s busy-aware LRU,
//! and unloads them before Ollama has to choose on its own.
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// Recent use decays with this half-life, so a model used heavily an hour ago counts less than
/// one used a few times in the last minutes.
const USE_HALF_LIFE: Duration = Duration::from_secs(30 * 60);
/// Assumed read throughput when a model has never been loaded here (bytes per second).
const ASSUMED_LOAD_BYTES_PER_SECOND: f64 = 1.5e9;
/// Exhaustive search over candidate sets stays cheap up to this many runners.
const MAX_EXACT_CANDIDATES: usize = 14;
/// Idle history is bounded; live leases are never discarded to satisfy the history limit.
const MAX_IDLE_HISTORY: usize = 1024;

#[derive(Debug, Default)]
struct Activity {
    /// Tasks routed to this model and not yet finished, queued ones included.
    active: usize,
    /// An unload owns the lifecycle until its request/confirmation finishes or is cancelled.
    evicting: bool,
    last_used: Option<Instant>,
    /// Exponentially decayed use count (half-life `USE_HALF_LIFE`) as of `decayed_at`.
    uses: f64,
    decayed_at: Option<Instant>,
    /// Runner-ready duration for observed transitions, including upstream scheduling/setup.
    load_ms: Option<f64>,
}

impl Activity {
    fn decay(&mut self, now: Instant) {
        if let Some(then) = self.decayed_at {
            let elapsed = now.saturating_duration_since(then).as_secs_f64();
            self.uses *= 0.5_f64.powf(elapsed / USE_HALF_LIFE.as_secs_f64());
        }
        self.decayed_at = Some(now);
    }
}

/// Per-model demand and load history, keyed by backend and model name.
#[derive(Default)]
struct ActivityState {
    entries: Mutex<HashMap<(String, String), Activity>>,
    changed: tokio::sync::Notify,
}

#[derive(Clone, Default)]
pub(crate) struct ModelActivity(Arc<ActivityState>);

/// Held while a task is routed to a model; dropping it marks the model idle again.
pub(super) struct ActiveModel {
    activity: ModelActivity,
    key: (String, String),
}

impl Drop for ActiveModel {
    fn drop(&mut self) {
        let mut inner = self.activity.lock();
        if let Some(entry) = inner.get_mut(&self.key) {
            entry.active = entry.active.saturating_sub(1);
        }
        trim_idle_history(&mut inner);
        drop(inner);
        self.activity.0.changed.notify_waiters();
    }
}

impl ActiveModel {
    /// Register demand before waiting, then wait outside all locks for an already-claimed
    /// unload. Call this before acquiring any backend transition guard. Dropping this future
    /// and its owner only releases sidecar demand; it does not prove upstream work stopped.
    pub(super) async fn ready(&self) {
        self.activity.ready_key(&self.key).await;
    }
}

/// Exclusive per-model lifecycle ownership. No mutex is held during network work.
pub(super) struct ModelEviction {
    activity: ModelActivity,
    key: (String, String),
}

impl Drop for ModelEviction {
    fn drop(&mut self) {
        let mut inner = self.activity.lock();
        if let Some(entry) = inner.get_mut(&self.key) {
            entry.evicting = false;
        }
        trim_idle_history(&mut inner);
        drop(inner);
        self.activity.0.changed.notify_waiters();
    }
}

fn trim_idle_history(entries: &mut HashMap<(String, String), Activity>) {
    if entries.len() <= MAX_IDLE_HISTORY {
        return;
    }
    let mut idle: Vec<_> = entries
        .iter()
        .filter(|(_, entry)| entry.active == 0 && !entry.evicting)
        .map(|(key, entry)| (key.clone(), entry.last_used))
        .collect();
    idle.sort_unstable_by_key(|(_, used)| *used);
    let remove = idle.len().saturating_sub(MAX_IDLE_HISTORY);
    for (key, _) in idle.into_iter().take(remove) {
        entries.remove(&key);
    }
}

/// What the planner knows about one model's recent use.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Usage {
    pub(super) active: usize,
    pub(super) evicting: bool,
    pub(super) idle_seconds: Option<f64>,
    pub(super) recent_uses: f64,
    pub(super) load_ms: Option<f64>,
}

impl ModelActivity {
    /// The caller must retain its registered `ActiveModel` while waiting, and already own a
    /// bounded admission slot. This method never registers an additional queue or demand.
    pub(super) async fn ready(&self, backend: &str, model: &str) {
        self.ready_key(&(backend.to_owned(), model.to_owned()))
            .await;
    }

    async fn ready_key(&self, key: &(String, String)) {
        loop {
            let changed = self.0.changed.notified();
            if !self.lock().get(key).is_some_and(|entry| entry.evicting) {
                return;
            }
            changed.await;
        }
    }
    /// A future that completes when a runner becomes ready or a model's use/unload claim ends. Create it
    /// before the state it will be compared against is read, so a change in between is not lost.
    pub(super) fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.changed.notified()
    }

    /// A load became a reusable runner while its task is still decoding. Capacity waiters must
    /// re-measure their demand: a same-model follower now needs no second model allocation.
    pub(super) fn runner_ready(&self) {
        self.0.changed.notify_waiters();
    }

    /// Restore only events whose callers observed a transition. Other audit rows, such as a slow
    /// response timer, are not physical-load evidence and are not restored.
    pub(super) fn seeded(store: Option<&crate::store::Store>) -> Self {
        let activity = Self::default();
        let events = store
            .and_then(|store| store.events_since(0, 1000).ok())
            .unwrap_or_default();
        for event in events.into_iter().rev() {
            if event.event == "load"
                && matches!(
                    event.trigger.as_str(),
                    "managed_transition_observed" | "warm_transition_observed"
                )
                && let Some(load_ms) = event.duration_ms
            {
                activity.learn_load(&event.backend, &event.model, load_ms);
            }
        }
        activity
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(String, String), Activity>> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register demand until the guard drops. Call `ready()` before backend locking/dispatch:
    /// registration protects queued tasks but does not overtake an already-owned unload.
    pub(super) fn begin(&self, backend: &str, model: &str) -> ActiveModel {
        self.register(backend, model, false)
            .expect("ordinary demand always registers")
    }

    /// Speculation must not wait behind an unload or existing demand for this model.
    pub(super) fn try_begin_idle(&self, backend: &str, model: &str) -> Option<ActiveModel> {
        self.register(backend, model, true)
    }

    fn register(&self, backend: &str, model: &str, idle_only: bool) -> Option<ActiveModel> {
        let key = (backend.to_owned(), model.to_owned());
        let now = Instant::now();
        let mut inner = self.lock();
        let entry = inner.entry(key.clone()).or_default();
        if idle_only && (entry.evicting || entry.active > 0) {
            return None;
        }
        entry.decay(now);
        entry.active += 1;
        entry.last_used = Some(now);
        trim_idle_history(&mut inner);
        drop(inner);
        Some(ActiveModel {
            activity: self.clone(),
            key,
        })
    }

    /// Test idle and claim its lifecycle in the same critical section as demand registration.
    pub(super) fn try_evict(&self, backend: &str, model: &str) -> Option<ModelEviction> {
        let key = (backend.to_owned(), model.to_owned());
        let mut inner = self.lock();
        let entry = inner.entry(key.clone()).or_default();
        if entry.active > 0 || entry.evicting {
            return None;
        }
        entry.evicting = true;
        trim_idle_history(&mut inner);
        Some(ModelEviction {
            activity: self.clone(),
            key,
        })
    }

    /// Explicit unload waits for existing demand without occupying the backend writer.
    pub(super) async fn evict_when_idle(&self, backend: &str, model: &str) -> ModelEviction {
        loop {
            let changed = self.0.changed.notified();
            if let Some(claim) = self.try_evict(backend, model) {
                return claim;
            }
            changed.await;
        }
    }

    /// Record a served task and an optional qualified runner-ready duration (zero means none).
    /// Only served work counts as demand; a task refused at admission never used the model.
    pub(super) fn completed(&self, backend: &str, model: &str, load_ms: u64) {
        let now = Instant::now();
        let mut inner = self.lock();
        let entry = inner
            .entry((backend.to_owned(), model.to_owned()))
            .or_default();
        entry.decay(now);
        entry.uses += 1.0;
        entry.last_used = Some(now);
        if load_ms > 0 {
            let sample = super::float(load_ms);
            entry.load_ms = Some(
                entry
                    .load_ms
                    .map_or(sample, |previous| previous * 0.7 + sample * 0.3),
            );
        }
        trim_idle_history(&mut inner);
    }

    /// Learn an independently observed transition duration, without counting a use.
    /// Callers must qualify the transition; a nonzero timer alone is insufficient.
    pub(super) fn learn_load(&self, backend: &str, model: &str, load_ms: u64) {
        if load_ms == 0 {
            return;
        }
        let sample = super::float(load_ms);
        let mut inner = self.lock();
        let entry = inner
            .entry((backend.to_owned(), model.to_owned()))
            .or_default();
        entry.load_ms = Some(
            entry
                .load_ms
                .map_or(sample, |previous| previous * 0.7 + sample * 0.3),
        );
        trim_idle_history(&mut inner);
    }

    pub(super) fn usage(&self, backend: &str, model: &str) -> Usage {
        let now = Instant::now();
        let mut inner = self.lock();
        let Some(entry) = inner.get_mut(&(backend.to_owned(), model.to_owned())) else {
            return Usage::default();
        };
        entry.decay(now);
        Usage {
            active: entry.active,
            evicting: entry.evicting,
            idle_seconds: entry
                .last_used
                .map(|then| now.saturating_duration_since(then).as_secs_f64()),
            recent_uses: entry.uses,
            load_ms: entry.load_ms,
        }
    }
}

/// Which memory an eviction is meant to free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freed {
    /// The whole runner (CPU backend or unified memory).
    Total,
    /// Discrete-GPU memory (`size_vram`).
    /// Pure planner coverage only until backend/device eligibility is established.
    #[cfg(test)]
    Vram,
    /// The part of a runner held in host RAM on a discrete-GPU machine (`size - size_vram`).
    HostSpill,
}

/// One resident runner that may be unloaded, with the cost of losing it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Candidate {
    pub(super) name: String,
    pub(super) bytes: u64,
    /// Expected reload time if this model is needed again.
    pub(super) reload_seconds: f64,
    pub(super) reload_source: &'static str,
    pub(super) recent_uses: f64,
    pub(super) idle_seconds: Option<f64>,
    /// Operator multiplier from `eviction_costs` (default 1).
    pub(super) weight: f64,
    /// Observed Ollama expiry, used as a local tiebreak.
    expires_at: String,
}

impl Candidate {
    /// Expected cost of unloading: the reload time, scaled by how likely the model is to be
    /// needed again soon, and by the operator's weight.
    pub(super) fn cost(&self) -> f64 {
        self.weight * self.reload_seconds.max(0.5) * (1.0 + self.recent_uses)
    }
}

/// The outcome of planning: runners to unload, and why others were left alone.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Plan {
    pub(super) victims: Vec<Candidate>,
    pub(super) skipped: Vec<(String, &'static str)>,
    pub(super) shortfall: u64,
    /// False when every eligible runner together would not free the shortfall.
    pub(super) covers_shortfall: bool,
}

impl Plan {
    pub(super) fn receipt(&self) -> Value {
        json!({
            "shortfall_bytes": self.shortfall,
            "covers_shortfall": self.covers_shortfall,
            "unloaded": self.victims.iter().map(|victim| json!({
                "model": victim.name,
                "freed_bytes": victim.bytes,
                "cost": round(victim.cost()),
                "reload_seconds": round(victim.reload_seconds),
                "reload_source": victim.reload_source,
                "recent_uses": round(victim.recent_uses),
                "idle_seconds": victim.idle_seconds.map(f64::round),
            })).collect::<Vec<_>>(),
            "kept": self.skipped.iter().map(|(model, reason)| json!({"model": model, "reason": reason})).collect::<Vec<_>>(),
        })
    }
}

pub(super) fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Inputs that are not part of `/api/ps`.
pub(super) struct PlanInputs<'a> {
    pub(super) backend: &'a str,
    /// The model about to load; never unloaded for itself.
    pub(super) keep: &'a str,
    pub(super) pinned: &'a BTreeSet<String>,
    pub(super) weights: &'a std::collections::BTreeMap<String, f64>,
    pub(super) freed: Freed,
    pub(super) shortfall: u64,
    /// Spare runners used within this many seconds: they are in use, only between requests, and
    /// unloading them would reload them on the next request. `None` spares none for recency.
    pub(super) min_idle_seconds: Option<f64>,
}

/// Choose the cheapest set of idle runners whose unloading frees at least `shortfall` bytes.
///
/// Never unloads the target, a pinned model, a `keep_alive: -1` runner, or a model with tasks
/// queued or running in `FreeLlama` (unloading it would only force an immediate reload). Among
/// the rest it minimises total cost, then the number of runners, then the bytes freed beyond
/// the shortfall; with too many runners for an exact search it falls back to cheapest-per-byte.
/// When even every eligible runner is not enough, all of them are returned and the plan says so:
/// the estimate is conservative, and the freed memory may still let the load fit (or, in VRAM,
/// spill fewer layers to the CPU).
pub(super) fn plan(ps: &Value, activity: &ModelActivity, inputs: &PlanInputs<'_>) -> Plan {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for entry in ps["models"].as_array().into_iter().flatten() {
        let Some(name) = crate::ollama_http::ps_entry_name(entry) else {
            continue;
        };
        let expires_at = entry
            .get("expires_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        let usage = activity.usage(inputs.backend, name);
        let reason = if name == inputs.keep {
            Some("target")
        } else if inputs.pinned.contains(name) {
            Some("pinned")
        } else if pinned_expiry(expires_at) {
            Some("keep_alive_forever")
        } else if expires_at.starts_with("0001-") {
            // Ollama reports the zero time while a runner is still loading for someone.
            Some("loading")
        } else if usage.active > 0 || usage.evicting {
            Some("busy")
        } else if inputs
            .min_idle_seconds
            .zip(usage.idle_seconds)
            .is_some_and(|(minimum, idle)| idle < minimum)
        {
            Some("recently_used")
        } else {
            None
        };
        if let Some(reason) = reason {
            if name != inputs.keep {
                skipped.push((name.to_owned(), reason));
            }
            continue;
        }
        let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
        let vram = entry.get("size_vram").and_then(Value::as_u64).unwrap_or(0);
        let bytes = match inputs.freed {
            Freed::Total => size,
            #[cfg(test)]
            Freed::Vram => vram,
            Freed::HostSpill => size.saturating_sub(vram),
        };
        if bytes == 0 {
            skipped.push((name.to_owned(), "frees_nothing_needed"));
            continue;
        }
        let (reload_seconds, reload_source) = usage.load_ms.map_or_else(
            || {
                (
                    super::float(size) / ASSUMED_LOAD_BYTES_PER_SECOND,
                    "size_estimate",
                )
            },
            |ms| (ms / 1000.0, "observed_runner_ready_duration"),
        );
        candidates.push(Candidate {
            name: name.to_owned(),
            bytes,
            reload_seconds,
            reload_source,
            recent_uses: usage.recent_uses,
            idle_seconds: usage.idle_seconds,
            weight: inputs.weights.get(name).copied().unwrap_or(1.0).max(0.0),
            expires_at: expires_at.to_owned(),
        });
    }
    let total: u64 = candidates.iter().map(|candidate| candidate.bytes).sum();
    let victims = if inputs.shortfall == 0 {
        Vec::new()
    } else if total < inputs.shortfall {
        candidates.clone()
    } else if candidates.len() <= MAX_EXACT_CANDIDATES {
        cheapest_covering_set(&candidates, inputs.shortfall)
    } else {
        greedy_covering_set(&candidates, inputs.shortfall)
    };
    let mut victims = victims;
    // Unload the least valuable first, so a partial failure frees the cheapest memory.
    victims.sort_by(|left, right| {
        left.cost()
            .total_cmp(&right.cost())
            .then_with(|| left.expires_at.cmp(&right.expires_at))
    });
    Plan {
        victims,
        skipped,
        shortfall: inputs.shortfall,
        covers_shortfall: total >= inputs.shortfall,
    }
}

fn cheapest_covering_set(candidates: &[Candidate], shortfall: u64) -> Vec<Candidate> {
    let mut best: Option<(f64, u32, u64, u32)> = None;
    let count = u32::try_from(candidates.len()).unwrap_or(0);
    for mask in 1_u32..(1 << count) {
        let mut cost = 0.0;
        let mut bytes = 0_u64;
        for (index, candidate) in candidates.iter().enumerate() {
            if mask & (1 << index) != 0 {
                cost += candidate.cost();
                bytes = bytes.saturating_add(candidate.bytes);
            }
        }
        if bytes < shortfall {
            continue;
        }
        let key = (cost, mask.count_ones(), bytes - shortfall, mask);
        let better = best.is_none_or(|current| {
            key.0
                .total_cmp(&current.0)
                .then(key.1.cmp(&current.1))
                .then(key.2.cmp(&current.2))
                .is_lt()
        });
        if better {
            best = Some(key);
        }
    }
    best.map_or_else(Vec::new, |(_, _, _, mask)| {
        candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, candidate)| candidate.clone())
            .collect()
    })
}

fn greedy_covering_set(candidates: &[Candidate], shortfall: u64) -> Vec<Candidate> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by(|left, right| {
        (left.cost() / super::float(left.bytes))
            .total_cmp(&(right.cost() / super::float(right.bytes)))
    });
    let mut freed = 0_u64;
    sorted
        .into_iter()
        .take_while(|candidate| {
            let needed = freed < shortfall;
            freed = freed.saturating_add(candidate.bytes);
            needed
        })
        .collect()
}

/// `keep_alive: -1` shows up in `/api/ps` as an expiry centuries away; treat anything more than
/// a year out as pinned by its owner.
fn pinned_expiry(expires_at: &str) -> bool {
    let Some(year) = expires_at
        .get(..4)
        .and_then(|year| year.parse::<u64>().ok())
    else {
        return false;
    };
    let now_year = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1970, |elapsed| 1970 + elapsed.as_secs() / 31_556_952);
    year > now_year + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::{
        future::Future as _,
        task::{Context, Waker},
    };

    #[tokio::test]
    async fn eviction_claim_and_arrival_are_atomic_in_both_orders() {
        let activity = ModelActivity::default();
        let running = activity.begin("gpu", "a");
        running.ready().await;
        assert!(activity.try_evict("gpu", "a").is_none());
        drop(running);

        let eviction = activity.try_evict("gpu", "a").unwrap();
        assert!(activity.try_evict("gpu", "a").is_none());
        assert!(activity.try_begin_idle("gpu", "a").is_none());
        let arriving = activity.begin("gpu", "a");
        {
            let mut ready = std::pin::pin!(arriving.ready());
            assert!(
                ready
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            // Another model and another backend remain independent.
            activity.begin("gpu", "b").ready().await;
            activity.begin("cpu", "a").ready().await;
            drop(eviction);
            ready.await;
        }
        assert!(activity.try_evict("gpu", "a").is_none());
        drop(arriving);
        assert!(activity.try_evict("gpu", "a").is_some());
    }

    #[tokio::test]
    async fn cancelled_arrival_releases_demand_and_cancelled_eviction_wakes_arrivals() {
        let activity = ModelActivity::default();
        let eviction = activity.try_evict("gpu", "a").unwrap();
        let waiting = activity.begin("gpu", "a");
        let mut pending = Box::pin(async move {
            waiting.ready().await;
            waiting
        });
        assert!(
            pending
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(activity.usage("gpu", "a").active, 1);
        drop(pending);
        assert_eq!(activity.usage("gpu", "a").active, 0);
        assert!(activity.usage("gpu", "a").evicting);
        let next = activity.begin("gpu", "a");
        drop(eviction); // Includes cancellation of an in-flight unload; no stop claim is made.
        next.ready().await;
        assert!(!activity.usage("gpu", "a").evicting);
    }

    #[test]
    fn idle_history_is_bounded_without_discarding_live_lifecycle_owners() {
        let activity = ModelActivity::default();
        let running = activity.begin("gpu", "running");
        let eviction = activity.try_evict("gpu", "evicting").unwrap();
        for index in 0..MAX_IDLE_HISTORY + 100 {
            activity.completed("gpu", &format!("idle-{index}"), 0);
        }
        assert_eq!(activity.lock().len(), MAX_IDLE_HISTORY + 2);
        assert_eq!(activity.usage("gpu", "running").active, 1);
        assert!(activity.usage("gpu", "evicting").evicting);
        drop((running, eviction));
        assert_eq!(activity.lock().len(), MAX_IDLE_HISTORY);
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn ps(entries: &[(&str, u64, u64, &str)]) -> Value {
        json!({"models": entries.iter().map(|(name, size, vram, expires)| json!({
            "name": name, "size": size, "size_vram": vram, "expires_at": expires,
        })).collect::<Vec<_>>()})
    }

    fn inputs<'a>(
        pinned: &'a BTreeSet<String>,
        weights: &'a BTreeMap<String, f64>,
        freed: Freed,
        shortfall: u64,
    ) -> PlanInputs<'a> {
        PlanInputs {
            backend: "gpu",
            keep: "target",
            pinned,
            weights,
            freed,
            shortfall,
            min_idle_seconds: None,
        }
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.victims
            .iter()
            .map(|victim| victim.name.as_str())
            .collect()
    }

    #[test]
    fn restart_restores_only_observed_transitions_as_load_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&directory.path().join("events.db")).unwrap();
        for (model, trigger, duration_ms) in [
            ("slow_timer", "managed_task", 30_000),
            ("observed", "managed_transition_observed", 50),
            ("raw_observed", "raw_transition_observed", 30_000),
            ("unverified_raw", "raw_transition_unverified", 30_000),
        ] {
            store
                .record_event(&crate::store::ResidencyEvent {
                    at_ms: 1,
                    backend: "gpu".into(),
                    model: model.into(),
                    event: "load".into(),
                    trigger: trigger.into(),
                    duration_ms: Some(duration_ms),
                    bytes: None,
                })
                .unwrap();
        }
        let activity = ModelActivity::seeded(Some(&store));
        assert_eq!(activity.usage("gpu", "slow_timer").load_ms, None);
        assert_eq!(activity.usage("gpu", "raw_observed").load_ms, None);
        assert_eq!(activity.usage("gpu", "unverified_raw").load_ms, None);
        assert_eq!(activity.usage("gpu", "observed").load_ms, Some(50.0));
        assert_eq!(activity.usage("gpu", "observed").recent_uses, 0.0);
    }

    #[test]
    fn one_cheap_runner_is_preferred_over_several_or_a_hot_one() {
        let activity = ModelActivity::default();
        // "hot" was used many times recently; "big-cold" is idle and big enough on its own.
        for _ in 0..6 {
            activity.completed("gpu", "hot", 0);
        }
        let loaded = ps(&[
            ("hot", 8 * GIB, 8 * GIB, "2026-09-29T12:05:00Z"),
            ("small-a", 2 * GIB, 2 * GIB, "2026-09-29T12:01:00Z"),
            ("small-b", 2 * GIB, 2 * GIB, "2026-09-29T12:02:00Z"),
            ("big-cold", 6 * GIB, 6 * GIB, "2026-09-29T12:03:00Z"),
        ]);
        let (pinned, weights) = (BTreeSet::new(), BTreeMap::new());
        let plan = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Vram, 5 * GIB),
        );
        assert!(plan.covers_shortfall);
        // Two small runners (4 GiB) are not enough; big-cold alone costs less than small-a +
        // small-b + anything, and far less than the hot model.
        assert_eq!(names(&plan), ["big-cold"]);
    }

    #[test]
    fn a_runner_in_use_between_requests_is_spared_when_recency_counts() {
        let activity = ModelActivity::default();
        // "ocr" just answered a request; "old" was never used through FreeLlama.
        activity.completed("gpu", "ocr", 0);
        let loaded = ps(&[
            ("ocr", 2 * GIB, 2 * GIB, "2026-09-29T12:05:00Z"),
            ("old", GIB, GIB, "2026-09-29T12:01:00Z"),
        ]);
        let (pinned, weights) = (BTreeSet::new(), BTreeMap::new());
        let spared = plan(
            &loaded,
            &activity,
            &PlanInputs {
                min_idle_seconds: Some(60.0),
                ..inputs(&pinned, &weights, Freed::Total, 3 * GIB)
            },
        );
        assert_eq!(names(&spared), ["old"]);
        assert!(
            spared
                .skipped
                .iter()
                .any(|(name, reason)| name == "ocr" && *reason == "recently_used")
        );
        // Without recency (swapping or OS pressure), the in-use runner is a candidate too.
        let any_idle = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Total, 3 * GIB),
        );
        assert!(names(&any_idle).contains(&"ocr"));
    }

    #[test]
    fn busy_pinned_target_and_forever_runners_are_never_unloaded() {
        let activity = ModelActivity::default();
        let _running = activity.begin("gpu", "busy");
        let loaded = ps(&[
            ("target", GIB, GIB, "2026-09-29T12:00:00Z"),
            ("busy", GIB, GIB, "2026-09-29T12:00:00Z"),
            ("pinned", GIB, GIB, "2026-09-29T12:00:00Z"),
            ("forever", GIB, GIB, "2318-01-01T00:00:00Z"),
            ("idle", GIB, GIB, "2026-09-29T12:00:00Z"),
        ]);
        let pinned = BTreeSet::from(["pinned".to_owned()]);
        let weights = BTreeMap::new();
        let plan = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Total, 10 * GIB),
        );
        assert_eq!(names(&plan), ["idle"]);
        assert!(!plan.covers_shortfall);
        let kept: Vec<_> = plan
            .skipped
            .iter()
            .map(|(name, reason)| (name.as_str(), *reason))
            .collect();
        assert_eq!(
            kept,
            [
                ("busy", "busy"),
                ("pinned", "pinned"),
                ("forever", "keep_alive_forever")
            ]
        );
    }

    #[test]
    fn measured_load_time_and_operator_weights_change_the_choice() {
        let activity = ModelActivity::default();
        // "slow" took 40s to load last time; "quick" loads in 1s. Same size, same idle time.
        activity.completed("gpu", "slow", 40_000);
        activity.completed("gpu", "quick", 1_000);
        let loaded = ps(&[
            ("slow", 4 * GIB, 4 * GIB, "2026-09-29T12:00:00Z"),
            ("quick", 4 * GIB, 4 * GIB, "2026-09-29T12:10:00Z"),
        ]);
        let (pinned, mut weights) = (BTreeSet::new(), BTreeMap::new());
        let chosen = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Vram, 3 * GIB),
        );
        assert_eq!(names(&chosen), ["quick"]);
        assert_eq!(
            chosen.victims[0].reload_source,
            "observed_runner_ready_duration"
        );
        weights.insert("quick".to_owned(), 100.0);
        let weighted = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Vram, 3 * GIB),
        );
        assert_eq!(names(&weighted), ["slow"]);
    }

    #[test]
    fn host_spill_counts_only_the_part_outside_vram() {
        let activity = ModelActivity::default();
        let loaded = ps(&[
            ("all-gpu", 8 * GIB, 8 * GIB, "2026-09-29T12:00:00Z"),
            ("spilled", 8 * GIB, 5 * GIB, "2026-09-29T12:00:00Z"),
        ]);
        let (pinned, weights) = (BTreeSet::new(), BTreeMap::new());
        let plan = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::HostSpill, 2 * GIB),
        );
        assert_eq!(names(&plan), ["spilled"]);
        assert_eq!(
            plan.skipped,
            [("all-gpu".to_owned(), "frees_nothing_needed")]
        );
    }

    #[test]
    fn runners_still_loading_are_never_unloaded() {
        let activity = ModelActivity::default();
        let loaded = ps(&[
            ("idle", 4 * GIB, 4 * GIB, "2026-09-29T12:00:00Z"),
            ("loading", 4 * GIB, 4 * GIB, "0001-01-01T00:00:00Z"),
        ]);
        let (pinned, weights) = (BTreeSet::new(), BTreeMap::new());
        let host = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Total, 40 * GIB),
        );
        assert_eq!(names(&host), ["idle"]);
        assert_eq!(host.skipped, [("loading".to_owned(), "loading")]);
    }

    #[test]
    fn many_runners_fall_back_to_cheapest_per_byte() {
        let activity = ModelActivity::default();
        let entries: Vec<(String, u64)> =
            (0..20).map(|index| (format!("m{index:02}"), GIB)).collect();
        let loaded = json!({"models": entries.iter().map(|(name, size)| json!({
            "name": name, "size": size, "size_vram": size, "expires_at": "2026-09-29T12:00:00Z",
        })).collect::<Vec<_>>()});
        let (pinned, weights) = (BTreeSet::new(), BTreeMap::new());
        let plan = plan(
            &loaded,
            &activity,
            &inputs(&pinned, &weights, Freed::Vram, 3 * GIB),
        );
        assert_eq!(plan.victims.len(), 3);
    }

    #[test]
    fn a_finished_task_releases_its_model() {
        let activity = ModelActivity::default();
        let guard = activity.begin("cpu", "m");
        assert_eq!(activity.usage("cpu", "m").active, 1);
        drop(guard);
        let usage = activity.usage("cpu", "m");
        assert_eq!(usage.active, 0);
        // Admitted but never served (refused, failed): no demand recorded.
        assert!(usage.recent_uses < 0.01);
        activity.completed("cpu", "m", 0);
        assert!(activity.usage("cpu", "m").recent_uses > 0.99);
        assert!(usage.idle_seconds.is_some());
        assert_eq!(activity.usage("gpu", "m"), Usage::default());
    }
}
