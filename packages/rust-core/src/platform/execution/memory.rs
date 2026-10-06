//! Memory admission for managed tasks: requirements, GPU budgets, eviction and KV preflight.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::super::error::{ApiError, resource_error};
use super::super::{
    Backend, CatalogModel, PlatformState, RouteDecision, get_json, host_has_unified_memory,
    host_total_memory_bytes, residency, resources,
};
use super::observe::request_unload;
use super::{ExecutionTarget, model_is_resident, resident_entry};

pub(crate) async fn model_memory_requirement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
) -> Value {
    let loopback = crate::net::is_loopback_url(&execution.upstream);
    let host_relevant = host_memory_relevant(&execution.upstream, execution.placement);
    // A discrete GPU holds the model in VRAM, but whatever does not fit spills into system RAM
    // ("mixed" placement). Estimate the full footprint, then charge host memory only for the
    // part the GPU's current free VRAM cannot hold.
    let discrete_gpu = loopback && !host_relevant;
    let current = if (host_relevant || discrete_gpu) && model.digest.is_some() {
        resident_entry(&state.client, &execution.upstream, &model.name).await
    } else {
        None
    };
    let context = decision.options["num_ctx"].as_u64().unwrap_or(0);
    let mut footprint = state.footprints.lock().await.requirement(
        &execution.upstream,
        model,
        context,
        current.as_ref(),
        host_relevant || discrete_gpu,
    );
    if discrete_gpu {
        let full = footprint["required_available_bytes"].as_u64().unwrap_or(0);
        if full > 0 {
            let observation = state.resources.snapshot().await.observation;
            apply_discrete_gpu_budget(&mut footprint, full, &observation);
        }
    }
    footprint
}

fn apply_discrete_gpu_budget(
    footprint: &mut Value,
    full: u64,
    observation: &resources::HostResources,
) {
    footprint["full_estimate_bytes"] = json!(full);
    if let Some(vram_free) = observation.gpu_memory_free_bytes {
        footprint["gpu_memory_free_bytes"] = json!(vram_free);
        footprint["gpu_telemetry_source"] = json!(observation.gpu_telemetry_source);
        footprint["required_available_bytes"] = json!(full.saturating_sub(vram_free));
        footprint["source"] = json!("discrete_gpu_spill_estimate");
    } else {
        // Unknown VRAM is not evidence that the GPU can hold any part of the runner. Charge
        // the full estimate to RAM so missing vendor tools cannot bypass host admission.
        footprint["required_available_bytes"] = json!(full);
        footprint["source"] = json!("discrete_gpu_unknown_vram_full_host_reservation");
    }
}

/// Size the load for `model` and, when it would not fit, unload idle runners first. Returns the
/// footprint (re-measured after any unload) and the eviction receipts.
///
/// Runs once per task, after it has a slot and before the resource timeouts start. It must not run
/// inside the 2-second slot-held reservation attempt: cancelling a multi-second unload halfway
/// loses the receipt, and a retry would plan a second round from telemetry that has not caught up,
/// unloading runners it does not need to.
pub(crate) async fn make_room(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
    deadline: tokio::time::Instant,
) -> (Value, Vec<Value>) {
    let mut evictions = Vec::new();
    let mut footprint = model_memory_requirement(state, execution, model, decision).await;
    if !state.tunables().evict_idle_models {
        return (footprint, evictions);
    }
    // Discrete GPU: make room in VRAM before Ollama must. Ollama would either evict by its
    // own least-recently-used order (possibly a model with work queued here) or spill layers
    // to the CPU; FreeLlama knows which runners are idle and cheap to reload.
    if let (Some(full), Some(free)) = (
        footprint["full_estimate_bytes"].as_u64(),
        footprint["gpu_memory_free_bytes"].as_u64(),
    ) {
        // A resident copy at a smaller context is replaced, so its VRAM comes back too.
        let own = resident_entry(&state.client, &execution.upstream, &decision.selected_model)
            .await
            .and_then(|entry| entry["size_vram"].as_u64())
            .unwrap_or(0);
        let shortfall = full.saturating_sub(free.saturating_add(own));
        // A model larger than the whole GPU spills whatever is unloaded; emptying VRAM for it
        // would only make every other model reload.
        let fits_the_gpu = state
            .resources
            .snapshot()
            .await
            .observation
            .gpu_memory_total_bytes
            .is_none_or(|total| full <= total);
        if shortfall > 0
            && fits_the_gpu
            && let Some(receipt) = evict_detached(
                state,
                execution,
                &decision.selected_model,
                residency::Freed::Vram,
                shortfall,
                deadline,
            )
            .await
        {
            evictions.push(receipt);
            footprint = model_memory_requirement(state, execution, model, decision).await;
        }
    }
    let bytes = footprint["required_available_bytes"].as_u64().unwrap_or(0);
    if bytes > 0 {
        let snapshot = state.resources.snapshot().await;
        // A host already holding for low memory recovers only above the resume reserve, and
        // idle runners are often exactly what holds that memory, so both cases evict.
        let assessment = snapshot.assess_capacity(bytes, snapshot.holding);
        let memory_hold = snapshot.reasons.iter().any(|reason| {
            matches!(
                reason,
                resources::PressureReason::LowAvailableMemory
                    | resources::PressureReason::OsMemoryPressure
                    | resources::PressureReason::ActiveSwapping
            )
        });
        let evict = match assessment.denial_reason {
            Some(resources::CapacityDenialReason::HostPressure) => memory_hold,
            // `LowMemoryMode::Refuse`: idle runners are often what holds the missing memory.
            Some(resources::CapacityDenialReason::InsufficientCapacity) => true,
            Some(_) => false,
            None => assessment.memory_advisory.is_some(),
        };
        if evict {
            let shortfall = assessment
                .required_with_reserve_bytes
                .unwrap_or(u64::MAX)
                .saturating_sub(assessment.effective_available_bytes.unwrap_or(0));
            let freed = if footprint["source"] == "discrete_gpu_spill_estimate" {
                residency::Freed::HostSpill
            } else {
                residency::Freed::Total
            };
            if let Some(receipt) = evict_detached(
                state,
                execution,
                &decision.selected_model,
                freed,
                shortfall,
                deadline,
            )
            .await
            {
                evictions.push(receipt);
                footprint = model_memory_requirement(state, execution, model, decision).await;
            }
        }
    }
    (footprint, evictions)
}

/// Run an eviction that a deadline can stop waiting for but never cancel halfway: the unloads,
/// the receipt and the telemetry refresh always complete together.
async fn evict_detached(
    state: &PlatformState,
    execution: &ExecutionTarget,
    keep: &str,
    freed: residency::Freed,
    shortfall: u64,
    deadline: tokio::time::Instant,
) -> Option<Value> {
    let (state, execution, keep) = (state.clone(), execution.clone(), keep.to_owned());
    let eviction = tokio::spawn(async move {
        evict_idle_models(&state, &execution, &keep, freed, shortfall).await
    });
    tokio::time::timeout_at(deadline, eviction)
        .await
        .ok()?
        .ok()?
}

pub(crate) async fn reserve_task_resources(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
    deadline: tokio::time::Instant,
    already_reserved: Option<u64>,
    sized: Option<Value>,
) -> Result<(resources::ResourcePermit, Value), ApiError> {
    let footprint = match sized {
        Some(footprint) => footprint,
        None => model_memory_requirement(state, execution, model, decision).await,
    };
    let bytes = footprint["required_available_bytes"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(already_reserved.unwrap_or(0));
    let resident = footprint["source"] == "matching_resident_context";
    if already_reserved.is_some() {
        // Refresh under the transition guard, but never wait for pressure recovery while holding
        // that guard: an unload request may be what makes recovery possible.
        state.resources.snapshot().await;
    }
    let demand = if resident && bytes == 0 {
        resources::ResourceDemand::resident()
    } else {
        resources::ResourceDemand::load(bytes)
    };
    let permit = state
        .resources
        .wait_for_demand(
            &execution.upstream,
            demand,
            if already_reserved.is_some() {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(tokio::time::Instant::now())
            },
        )
        .await
        .map_err(resource_error)?;
    Ok((permit, footprint))
}

/// Unload the cheapest set of idle runners on `execution` that frees about `shortfall` bytes.
///
/// A cold load can need memory that only idle resident models are holding; without unloading
/// them the request would hold its slot and then fail with 503. The victims come
/// from `residency::plan`: never the target, a pinned or `keep_alive: -1` model, or one with
/// tasks queued or running here; among the rest, the set whose loss costs least (measured reload
/// time, recent demand, operator weight). `keep_alive: 0` is safe for a runner that is still
/// serving a raw client: Ollama unloads it once those requests finish.
pub(crate) async fn evict_idle_models(
    state: &PlatformState,
    execution: &ExecutionTarget,
    keep: &str,
    freed: residency::Freed,
    shortfall: u64,
) -> Option<Value> {
    let ps = get_json(&state.client, &execution.upstream, "/api/ps")
        .await
        .ok()?;
    let save = state
        .footprints
        .lock()
        .await
        .observe_resident(&execution.upstream, &ps);
    if let Some(save) = save {
        save.persist().await;
    }
    let tunables = state.tunables();
    let plan = residency::plan(
        &ps,
        &state.activity,
        &residency::PlanInputs {
            backend: execution.placement.as_str(),
            keep,
            pinned: &tunables.pinned_models,
            weights: &tunables.eviction_costs,
            freed,
            shortfall,
        },
    );
    let mut evicted = Vec::new();
    let mut claimed = Vec::new();
    for victim in &plan.victims {
        // A task may have been routed to this runner since the plan was made.
        if state
            .activity
            .usage(execution.placement.as_str(), &victim.name)
            .active
            > 0
        {
            claimed.push(victim.name.clone());
            continue;
        }
        if request_unload(state, &execution.upstream, &victim.name)
            .await
            .is_ok_and(|(status, _)| status.is_success())
        {
            evicted.push(victim.name.clone());
        }
    }
    // Give Ollama a moment to release the runners before capacity is re-read.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !evicted.is_empty() && Instant::now() < deadline {
        let mut still_loaded = false;
        for name in &evicted {
            if model_is_resident(&state.client, &execution.upstream, name).await {
                still_loaded = true;
                break;
            }
        }
        if !still_loaded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if plan.victims.is_empty() {
        return None;
    }
    state
        .telemetry
        .record_evictions(execution.placement.as_str(), evicted.len());
    if !evicted.is_empty() {
        // The cached sample predates the unload; the next capacity check must re-read.
        state.resources.invalidate().await;
    }
    let mut receipt = plan.receipt();
    receipt["backend"] = json!(execution.placement);
    receipt["for_model"] = json!(keep);
    receipt["memory"] = json!(match freed {
        residency::Freed::Total => "host",
        residency::Freed::Vram => "vram",
        residency::Freed::HostSpill => "host_spill",
    });
    receipt["unloaded_ok"] = json!(evicted);
    receipt["claimed_before_unload"] = json!(claimed);
    receipt["at"] = json!(super::super::telemetry::now_seconds());
    *state
        .last_eviction
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(receipt.clone());
    Some(receipt)
}

/// Host RAM is charged for a loopback backend that runs on the CPU or shares unified memory.
pub(crate) fn host_memory_relevant(upstream: &str, placement: Backend) -> bool {
    crate::net::is_loopback_url(upstream)
        && (placement == Backend::Cpu || host_has_unified_memory())
}

/// Advisory F16 estimate and a coarse local model-size budget, not Ollama's live loader check.
/// Cache precision and runner parallelism belong to the selected Ollama process, so reading the
/// gateway's environment cannot establish either. An F16 estimate can exceed actual quantized
/// cache usage and must not be presented as a known minimum or used to reject that configuration.
pub(crate) fn memory_kv_preflight(
    model: &CatalogModel,
    route: &RouteDecision,
    placement: Backend,
    upstream: &str,
) -> Value {
    let requested_context = route
        .options
        .get("num_ctx")
        .and_then(Value::as_u64)
        .or(model.advertised_context);
    let host_relevant = host_memory_relevant(upstream, placement);
    memory_kv_preflight_with_memory(
        model,
        requested_context,
        host_relevant,
        host_total_memory_bytes(),
    )
}

pub(crate) fn memory_kv_preflight_with_memory(
    model: &CatalogModel,
    requested_context: Option<u64>,
    host_relevant: bool,
    host_memory_bytes: Option<u64>,
) -> Value {
    let kv_cache_bytes = model
        .kv_cache_bytes_per_token_f16
        .zip(requested_context)
        .and_then(|(per_token, context)| per_token.checked_mul(context));
    let model_plus_kv_bytes = kv_cache_bytes.and_then(|kv| model.size.checked_add(kv));
    let host_budget = host_memory_bytes.map(|total| total / 5 * 4);
    let refuses = host_relevant && host_budget.is_some_and(|budget| model.size > budget);
    let estimate_exceeds_budget = host_relevant
        && model_plus_kv_bytes
            .zip(host_budget)
            .is_some_and(|(needed, budget)| needed > budget);
    let status = if refuses {
        "refuse_model_file_exceeds_host_budget"
    } else if estimate_exceeds_budget {
        "f16_estimate_exceeds_host_budget"
    } else if kv_cache_bytes.is_some() {
        "estimated_f16_single_sequence"
    } else {
        "unknown_model_metadata"
    };
    json!({
        "status": status,
        "refuses": refuses,
        "known_model_bytes": model.size,
        "requested_context_tokens": requested_context,
        "kv_cache_bytes_f16_estimate": kv_cache_bytes,
        "model_plus_kv_bytes_f16_estimate": model_plus_kv_bytes,
        "host_memory_bytes": host_memory_bytes,
        "host_budget_bytes": host_budget,
        "host_memory_relevant": host_relevant,
        "threshold": "model_file_only_80_percent_of_total_host_memory_when_loopback_cpu_or_unified",
        "assumptions": "unquantized F16, one sequence, uniform full attention, no padding or runner overhead; loopback assumed local",
        "upstream_parallelism": "unknown",
        "upstream_kv_cache_type": "unknown",
        "live_available_memory_bytes": null,
        "authority": "Ollama owns live free-memory, runner graph, cache-type, and final load admission",
    })
}

#[cfg(test)]
mod gpu_budget_tests {
    use super::*;

    #[test]
    fn missing_vram_telemetry_reserves_the_full_footprint() {
        let mut footprint = json!({"required_available_bytes": 8000});
        apply_discrete_gpu_budget(&mut footprint, 8000, &resources::HostResources::default());
        assert_eq!(footprint["required_available_bytes"], 8000);
    }

    #[test]
    fn observed_vram_only_discounts_memory_it_can_hold() {
        for (free, expected) in [(0, 8000), (3000, 5000), (9000, 0)] {
            let mut footprint = json!({"required_available_bytes": 8000});
            let observation = resources::HostResources {
                gpu_memory_free_bytes: Some(free),
                ..resources::HostResources::default()
            };
            apply_discrete_gpu_budget(&mut footprint, 8000, &observation);
            assert_eq!(footprint["required_available_bytes"], expected);
        }
    }
}
