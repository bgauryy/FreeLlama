//! Release idle runners when the host runs short of memory, without waiting for the next load.
//!
//! Load-time eviction frees room only when a new model needs it, and a pressure hold only
//! shortens future `keep_alive` values, so an idle host under memory pressure (a build started
//! after the models loaded) kept every runner until its timer ran out. This task watches only
//! while a runner is resident: with nothing loaded it asks Ollama's `/api/ps` (no host sampling),
//! and with something loaded it reads the resource governor at its own sampling cadence. Under a
//! memory hold it unloads the cheapest idle runners, chosen by the same planner as load-time
//! eviction, so pinned, `keep_alive: -1` and busy models stay.
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
                    let receipt = execution::evict_idle_models(
                        &state,
                        &execution,
                        "",
                        residency::Freed::Total,
                        shortfall,
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

#[cfg(test)]
mod tests {
    use super::*;

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
