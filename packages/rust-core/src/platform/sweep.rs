//! Release idle runners when the host runs short of memory, without waiting for the next load.
//!
//! Load-time eviction frees room only when a new model needs it, and a pressure hold only
//! shortens future `keep_alive` values, so an idle host under memory pressure (a build started
//! after the models loaded) kept every runner until its timer ran out. This task watches only
//! while a runner is resident: with nothing loaded it asks Ollama's `/api/ps` (no host sampling),
//! and with something loaded it reads the resource governor at its own sampling cadence. Under a
//! memory hold it unloads the cheapest idle runners, chosen by the same planner as load-time
//! eviction, so pinned, `keep_alive: -1` and busy models stay.
//!
//! Other clients share the runners: a model another program calls every second has no task here,
//! but Ollama moves its `expires_at` forward on every request. The sweep compares each runner's
//! expiry with its previous check and records a moved one as recent use, so the idle floor keeps
//! it. Swapping or OS pressure lowers that floor to `warming.min_seconds` instead of removing it: a
//! runner used seconds ago is reloaded at once, so unloading it frees memory only briefly.
use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;

use super::resources::PressureReason;
use super::{Backend, PlatformState, catalog, execution, residency};

/// How many governor sampling intervals pass between checks while runners are resident.
const CHECKS_EVERY_SAMPLES: u32 = 5;
/// How many checks pass between `/api/ps` reads while nothing is resident.
const IDLE_BACKOFF: u32 = 3;

pub(super) fn start(state: &PlatformState) {
    if !state.tunables().evict_idle_models {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut idle_rounds = 0;
        let mut expiries = Expiries::default();
        loop {
            let interval = state.resources.policy().sample_interval * CHECKS_EVERY_SAMPLES;
            let pause = if idle_rounds > 0 {
                interval * IDLE_BACKOFF
            } else {
                interval
            };
            tokio::time::sleep(pause).await;
            if !state.tunables().evict_idle_models {
                continue;
            }
            let mut any_resident = false;
            for execution in targets(&state) {
                let Ok(ps) = catalog::get_json(&state.client, &execution.upstream, "/api/ps").await
                else {
                    continue;
                };
                if ps["models"].as_array().is_none_or(Vec::is_empty) {
                    continue;
                }
                any_resident = true;
                for model in expiries.moved(&execution.upstream, &ps) {
                    state.activity.note_used_elsewhere(
                        execution.placement.as_str(),
                        &model.name,
                        model.since,
                    );
                }
                if !execution::host_memory_relevant(&execution.upstream, execution.placement) {
                    continue;
                }
                let snapshot = state.resources.snapshot().await;
                let shortfall = memory_shortfall(
                    snapshot.holding,
                    &snapshot.reasons,
                    snapshot.effective_available_bytes,
                    snapshot.resume_reserve_bytes,
                    snapshot.hold_reserve_bytes,
                );
                if shortfall > 0 {
                    // FreeLlama's own low-memory estimate alone is no distress: release only
                    // runners that have gone idle, not one serving requests a few seconds apart.
                    // Swapping or OS pressure shortens that wait to `warming.min_seconds`.
                    let distress = snapshot.reasons.iter().any(|reason| {
                        matches!(
                            reason,
                            PressureReason::ActiveSwapping | PressureReason::OsMemoryPressure
                        )
                    });
                    // The longest an unused runner would be kept under a memory hold
                    // (`base_seconds × pressure_factor`, 75 s by default), not the unpressured
                    // base: that matches Ollama's five-minute default, and a runner idle that
                    // long would expire on its own before this sweep released it.
                    let warming = &state.tunables().warming;
                    let min_idle_seconds = Some(if distress {
                        super::float(warming.min_seconds)
                    } else {
                        super::float(warming.base_seconds) * warming.pressure_factor
                    });
                    let receipt = execution::evict_idle_models(
                        &state,
                        &execution,
                        "",
                        residency::Freed::Total,
                        shortfall,
                        min_idle_seconds,
                        "pressure_sweep",
                    )
                    .await;
                    if let Some(receipt) = receipt
                        && receipt["unloaded_ok"]
                            .as_array()
                            .is_some_and(|unloaded| !unloaded.is_empty())
                    {
                        eprintln!(
                            "freellama: memory pressure ({:?}); unloaded idle {}",
                            snapshot.reasons, receipt["unloaded_ok"]
                        );
                    }
                }
            }
            idle_rounds = if any_resident { 0 } else { idle_rounds + 1 };
        }
    });
}

fn targets(state: &PlatformState) -> Vec<execution::ExecutionTarget> {
    let mut targets = vec![execution::primary_target(state)];
    if state.cpu_upstream.is_some() {
        // Any CPU-assigned model resolves to the CPU backend's target.
        if let Some(model) = state.cpu_models.iter().next() {
            let target = execution::execution_target(state, model);
            if target.placement == Backend::Cpu {
                targets.push(target);
            }
        }
    }
    targets
}

/// Bytes to free so available memory reaches the resume reserve, while a memory signal holds the
/// host; zero otherwise. CPU load and heat are not freed by unloading an idle runner.
fn memory_shortfall(
    holding: bool,
    reasons: &[PressureReason],
    available: Option<u64>,
    resume_reserve: u64,
    hold_reserve: u64,
) -> u64 {
    let memory_hold = holding
        && reasons.iter().any(|reason| {
            matches!(
                reason,
                PressureReason::LowAvailableMemory
                    | PressureReason::OsMemoryPressure
                    | PressureReason::ActiveSwapping
            )
        });
    if !memory_hold {
        return 0;
    }
    // Swapping or OS pressure can hold a host whose available figure still clears the reserve;
    // free at least one reserve's worth then, or the hold never lifts.
    resume_reserve
        .saturating_sub(available.unwrap_or(0))
        .max(hold_reserve)
}

/// A runner whose Ollama expiry moved forward since the previous check.
#[derive(Debug, PartialEq)]
struct Moved {
    name: String,
    /// When the previous check saw the old expiry: the use happened after this.
    since: Instant,
}

/// Each resident runner's last observed `expires_at`, per upstream, with when it was observed.
#[derive(Default)]
struct Expiries(HashMap<(String, String), (String, Instant)>);

impl Expiries {
    /// Record this `/api/ps` reading and return the runners whose expiry changed since the last
    /// one. A first sighting, a runner still loading (`0001-…`) and a pinned one report nothing;
    /// runners no longer listed are forgotten.
    fn moved(&mut self, upstream: &str, ps: &Value) -> Vec<Moved> {
        let now = Instant::now();
        let mut seen = HashMap::new();
        let mut moved = Vec::new();
        for entry in ps["models"].as_array().into_iter().flatten() {
            let Some(name) = crate::ollama_http::ps_entry_name(entry) else {
                continue;
            };
            let Some(expires_at) = entry.get("expires_at").and_then(Value::as_str) else {
                continue;
            };
            let key = (upstream.to_owned(), name.to_owned());
            if let Some((previous, observed)) = self.0.get(&key)
                && previous != expires_at
                && !expires_at.starts_with("0001-")
            {
                moved.push(Moved {
                    name: name.to_owned(),
                    since: *observed,
                });
            }
            seen.insert(key, (expires_at.to_owned(), now));
        }
        self.0.retain(|(owner, _), _| owner != upstream);
        self.0.extend(seen);
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ps(entries: &[(&str, &str)]) -> Value {
        json!({"models": entries.iter().map(|(name, expires)| json!({"name": name, "model": name, "expires_at": expires})).collect::<Vec<_>>()})
    }

    #[test]
    fn a_moved_expiry_is_use_by_another_client() {
        let mut expiries = Expiries::default();
        let upstream = "http://127.0.0.1:11434";
        // A first sighting proves nothing about use.
        assert_eq!(
            expiries.moved(
                upstream,
                &ps(&[
                    ("a:1b", "2026-11-14T10:05:00Z"),
                    ("b:1b", "2026-11-14T10:05:00Z")
                ])
            ),
            []
        );
        let moved = expiries.moved(
            upstream,
            &ps(&[
                ("a:1b", "2026-11-14T10:05:03Z"),
                ("b:1b", "2026-11-14T10:05:00Z"),
            ]),
        );
        assert_eq!(
            moved.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["a:1b"]
        );
        // A runner reloading reports Ollama's zero time, which is not a use.
        assert_eq!(
            expiries.moved(upstream, &ps(&[("a:1b", "0001-01-01T00:00:00Z")])),
            []
        );
        // Gone, then back: a first sighting again.
        assert_eq!(expiries.moved(upstream, &ps(&[])), []);
        assert_eq!(
            expiries.moved(upstream, &ps(&[("a:1b", "2026-11-14T10:09:00Z")])),
            []
        );
    }

    #[test]
    fn upstreams_are_tracked_apart() {
        let mut expiries = Expiries::default();
        let entry = ps(&[("a:1b", "2026-11-14T10:05:00Z")]);
        expiries.moved("http://gpu", &entry);
        expiries.moved("http://cpu", &ps(&[("a:1b", "2026-11-14T11:00:00Z")]));
        assert_eq!(expiries.moved("http://gpu", &entry), []);
    }

    #[test]
    fn only_memory_holds_free_runners() {
        const GIB: u64 = 1 << 30;
        let cpu_and_heat = [
            PressureReason::HighCpuLoad,
            PressureReason::ThermalThrottling,
        ];
        assert_eq!(
            memory_shortfall(true, &cpu_and_heat, Some(GIB), 4 * GIB, 2 * GIB),
            0
        );
        let swapping = [PressureReason::ActiveSwapping];
        assert_eq!(
            memory_shortfall(false, &swapping, Some(GIB), 4 * GIB, 2 * GIB),
            0
        );
        assert_eq!(
            memory_shortfall(true, &swapping, Some(GIB), 4 * GIB, 2 * GIB),
            3 * GIB
        );
        // Plenty available but the OS still reports pressure: free one hold reserve.
        assert_eq!(
            memory_shortfall(true, &swapping, Some(10 * GIB), 4 * GIB, 2 * GIB),
            2 * GIB
        );
    }
}
