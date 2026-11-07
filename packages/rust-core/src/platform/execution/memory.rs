//! Memory admission for managed tasks: requirements, GPU budgets, eviction and KV preflight.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::super::error::{ApiError, resource_error};
use super::super::{
    Backend, CatalogModel, PlatformState, RouteDecision, budget, get_json, host_has_unified_memory,
    host_total_memory_bytes, residency, resources,
};
use super::observe::request_unload;
use super::{ExecutionTarget, resident_entry};

/// The context size a route asks the runner for; 0 when it leaves the size to Ollama.
pub(crate) fn requested_context(decision: &RouteDecision) -> u64 {
    decision.options["num_ctx"].as_u64().unwrap_or(0)
}

pub(crate) async fn model_memory_requirement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    context: u64,
) -> Value {
    model_memory_requirement_with_reuse(state, execution, model, context, true, None).await
}

pub(crate) async fn cold_model_memory_requirement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    context: u64,
) -> Value {
    model_memory_requirement_with_reuse(state, execution, model, context, false, None).await
}

/// `observed` is a `/api/ps` listing the caller read an instant ago; the forecast then reads
/// nothing itself. Without one, it reads residency when a matching runner could shrink the load.
async fn model_memory_requirement_with_reuse(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    context: u64,
    allow_resident_reuse: bool,
    observed: Option<&Value>,
) -> Value {
    let loopback = crate::net::is_loopback_url(&execution.upstream);
    let host_relevant = host_memory_relevant(&execution.upstream, execution.placement);
    // Vendor-wide free VRAM does not prove which devices this Ollama process can allocate on.
    // Keep the full host forecast until backend/device eligibility can be established.
    let discrete_gpu = loopback && !host_relevant;
    let current =
        if allow_resident_reuse && (host_relevant || discrete_gpu) && model.digest.is_some() {
            match observed {
                Some(ps) => crate::ollama_http::find_ps_entry_by_digest(
                    ps,
                    &model.name,
                    model.digest.as_deref(),
                )
                .cloned(),
                None => {
                    resident_entry(
                        &state.client,
                        &execution.upstream,
                        &model.name,
                        model.digest.as_deref(),
                    )
                    .await
                }
            }
        } else {
            None
        };
    let placement_unverified = execution.placement == Backend::Cpu
        && current
            .as_ref()
            .is_some_and(|entry| entry["size_vram"].as_u64() != Some(0));
    let current = current.filter(|_| !placement_unverified);
    let mut footprint = state.footprints.lock().await.requirement(
        &execution.upstream,
        model,
        context,
        current.as_ref(),
        host_relevant || discrete_gpu,
    );
    if !allow_resident_reuse {
        footprint["resident_reuse"] = json!("unverified_load_options_full_forecast");
    } else if placement_unverified {
        footprint["resident_reuse"] = json!("unverified_cpu_placement_full_forecast");
    }
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
    footprint["required_available_bytes"] = json!(full);
    if let Some(vram_free) = observation.gpu_memory_free_bytes {
        footprint["gpu_memory_free_bytes"] = json!(vram_free);
        footprint["gpu_telemetry_source"] = json!(observation.gpu_telemetry_source);
        footprint["source"] =
            json!("discrete_gpu_unverified_device_eligibility_full_host_reservation");
        footprint["gpu_capacity_credit_bytes"] = json!(0);
        footprint["gpu_capacity_scope"] = json!("host_aggregate_not_verified_for_backend");
    } else {
        // Unknown VRAM is not evidence that the GPU can hold any part of the runner. Charge
        // the full estimate to RAM so missing vendor tools cannot bypass host admission.
        footprint["required_available_bytes"] = json!(full);
        footprint["source"] = json!("discrete_gpu_unknown_vram_full_host_reservation");
    }
}

/// Size the load for `model` at `context` and, when it would not fit, unload idle runners first.
/// Three limits can each ask for room, in this order: a discrete GPU's free VRAM, the model memory
/// budget, and host memory pressure. Returns the footprint (re-measured after any unload) and the
/// eviction receipts. Raw passthrough loads use the same path.
///
/// Runs once per task, after it has a slot and before the resource timeouts start. It must not run
/// inside the 2-second slot-held reservation attempt: cancelling a multi-second unload halfway
/// loses the receipt, and a retry would plan a second round from telemetry that has not caught up,
/// unloading runners it does not need to.
pub(crate) async fn make_room(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    context: u64,
    deadline: tokio::time::Instant,
) -> (Value, Vec<Value>) {
    let mut evictions = Vec::new();
    // One residency read serves the forecast and the budget check below.
    let observed = observe_residency(state, execution, model).await;
    let mut footprint = model_memory_requirement_with_reuse(
        state,
        execution,
        model,
        context,
        true,
        observed.as_ref(),
    )
    .await;
    if !state.tunables().evict_idle_models {
        return (footprint, evictions);
    }
    // GPU totals remain monitoring data until the selected backend's eligible devices are
    // verified. Do not evict or credit capacity against a fictional aggregate device pool.
    if let Some(receipt) = keep_within_budget(
        state,
        execution,
        &model.name,
        &footprint,
        observed.as_ref(),
        deadline,
    )
    .await
    {
        evictions.push(receipt);
        footprint = model_memory_requirement(state, execution, model, context).await;
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
            let freed = if crate::net::is_loopback_url(&execution.upstream)
                && !host_memory_relevant(&execution.upstream, execution.placement)
            {
                residency::Freed::HostSpill
            } else {
                residency::Freed::Total
            };
            if let Some(receipt) = evict_detached(
                state,
                execution,
                &model.name,
                freed,
                shortfall,
                deadline,
                "host_memory",
            )
            .await
            {
                evictions.push(receipt);
                footprint = model_memory_requirement(state, execution, model, context).await;
            }
        }
    }
    (footprint, evictions)
}

/// The residency listing the forecast and the budget check of `make_room` both need, or `None`
/// when neither reads it (a remote backend, or no digest and no host memory to budget).
async fn observe_residency(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
) -> Option<Value> {
    let local = crate::net::is_loopback_url(&execution.upstream);
    if !local
        || (model.digest.is_none()
            && !host_memory_relevant(&execution.upstream, execution.placement))
    {
        return None;
    }
    get_json(&state.client, &execution.upstream, "/api/ps")
        .await
        .ok()
}

/// Unload idle runners so that loading `keep` leaves the backend's resident total within the model
/// memory budget. Applies where the runners hold host memory (a CPU backend or unified memory);
/// a load already resident at a compatible context needs nothing.
async fn keep_within_budget(
    state: &PlatformState,
    execution: &ExecutionTarget,
    keep: &str,
    footprint: &Value,
    observed: Option<&Value>,
    deadline: tokio::time::Instant,
) -> Option<Value> {
    let load = footprint["required_available_bytes"].as_u64().unwrap_or(0);
    if load == 0 || !host_memory_relevant(&execution.upstream, execution.placement) {
        return None;
    }
    let budget = state
        .tunables()
        .model_memory_budget
        .bytes(host_total_memory_bytes())?;
    let fetched;
    let ps = if let Some(ps) = observed {
        ps
    } else {
        fetched = get_json(&state.client, &execution.upstream, "/api/ps")
            .await
            .ok()?;
        &fetched
    };
    let over = budget::overage(budget, budget::resident_bytes(ps, keep), load);
    if over == 0 {
        return None;
    }
    let mut receipt = evict_detached(
        state,
        execution,
        keep,
        residency::Freed::Total,
        over,
        deadline,
        "model_memory_budget",
    )
    .await?;
    receipt["budget_bytes"] = json!(budget);
    Some(receipt)
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
    trigger: &'static str,
) -> Option<Value> {
    let (state, execution, keep) = (state.clone(), execution.clone(), keep.to_owned());
    let eviction = tokio::spawn(async move {
        evict_idle_models(&state, &execution, &keep, freed, shortfall, None, trigger).await
    });
    tokio::time::timeout_at(deadline, eviction)
        .await
        .ok()?
        .ok()?
}

/// How `reserve_task_resources` obtains the load's memory forecast.
pub(crate) enum Forecast<'a> {
    /// Measure now, reading residency from Ollama.
    Measure,
    /// The caller already sized this load for the same options.
    Sized(Value),
    /// Measure against a residency listing the caller read an instant ago.
    Observed(&'a Value),
}

pub(crate) async fn reserve_task_resources(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &CatalogModel,
    decision: &RouteDecision,
    deadline: tokio::time::Instant,
    already_reserved: Option<u64>,
    forecast: Forecast<'_>,
) -> Result<(resources::ResourcePermit, Value), ApiError> {
    let mut effective = json!({"options": decision.options});
    super::apply_execution_options(&mut effective, execution);
    let unverified_options = super::runner::RunnerExpectation::new(model, decision)
        .with_runtime_options(&effective["options"])
        .has_unverified_options();
    let footprint = match forecast {
        Forecast::Sized(footprint) if !unverified_options => footprint,
        Forecast::Observed(ps) => {
            model_memory_requirement_with_reuse(
                state,
                execution,
                model,
                requested_context(decision),
                !unverified_options,
                Some(ps),
            )
            .await
        }
        Forecast::Measure | Forecast::Sized(_) => {
            model_memory_requirement_with_reuse(
                state,
                execution,
                model,
                requested_context(decision),
                !unverified_options,
                None,
            )
            .await
        }
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
    // A CPU-assigned backend decodes on the host cores, so CPU saturation holds it; a resident
    // GPU runner is held only by memory pressure and thermal signals.
    let cpu_bound = execution.placement == Backend::Cpu;
    let demand = if resident && bytes == 0 {
        resources::ResourceDemand::resident()
    } else {
        resources::ResourceDemand::load(bytes)
    }
    .on_cpu(cpu_bound);
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
    min_idle_seconds: Option<f64>,
    trigger: &'static str,
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
            min_idle_seconds,
        },
    );
    if plan.victims.is_empty() {
        return None;
    }
    let unloaded = unload_victims(state, execution, &plan.victims, trigger).await;
    state
        .telemetry
        .record_evictions(execution.placement.as_str(), unloaded.confirmed.len());
    let mut receipt = plan.receipt();
    receipt["planned_evictions"] = receipt["unloaded"].clone();
    receipt["unloaded"] = json!(
        receipt["planned_evictions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["model"]
                .as_str()
                .is_some_and(|name| unloaded.confirmed.iter().any(|model| model == name)))
            .cloned()
            .collect::<Vec<_>>()
    );
    receipt["backend"] = json!(execution.placement);
    receipt["for_model"] = json!(keep);
    receipt["trigger"] = json!(trigger);
    receipt["unload_ms"] = json!(unloaded.elapsed_ms);
    receipt["memory"] = json!(match freed {
        residency::Freed::Total => "host",
        #[cfg(test)]
        residency::Freed::Vram => "vram",
        residency::Freed::HostSpill => "host_spill",
    });
    receipt["unload_requested"] = json!(unloaded.requested);
    receipt["unloaded_ok"] = json!(unloaded.confirmed);
    receipt["still_resident"] = json!(unloaded.still_resident);
    receipt["observation_failed"] = json!(unloaded.observation_failed);
    receipt["unload_failed"] = json!(unloaded.failed);
    receipt["claimed_before_unload"] = json!(unloaded.claimed);
    receipt["at"] = json!(super::super::telemetry::now_seconds());
    *state
        .last_eviction
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(receipt.clone());
    Some(receipt)
}

#[derive(Default)]
struct UnloadOutcome {
    requested: Vec<String>,
    confirmed: Vec<String>,
    claimed: Vec<String>,
    failed: Vec<String>,
    still_resident: Vec<String>,
    observation_failed: Vec<String>,
    elapsed_ms: u64,
}

/// Unload the planned victims that are still idle, then confirm their absence within one bounded
/// observation window. HTTP acceptance alone is never recorded as a completed eviction.
async fn unload_victims(
    state: &PlatformState,
    execution: &ExecutionTarget,
    victims: &[residency::Candidate],
    trigger: &'static str,
) -> UnloadOutcome {
    let unloading = Instant::now();
    let mut outcome = UnloadOutcome::default();
    let mut requested_at = std::collections::BTreeMap::new();
    let mut claims = Vec::new();
    let request_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    for victim in victims {
        // Claim idle atomically with arrivals; hold the lease through bounded confirmation.
        let Some(claim) = state
            .activity
            .try_evict(execution.placement.as_str(), &victim.name)
        else {
            outcome.claimed.push(victim.name.clone());
            continue;
        };
        claims.push(claim);
        let started = Instant::now();
        if tokio::time::timeout_at(
            request_deadline,
            request_unload(state, &execution.upstream, &victim.name),
        )
        .await
        .is_ok_and(|result| result.is_ok_and(|(status, _)| status.is_success()))
        {
            outcome.requested.push(victim.name.clone());
            requested_at.insert(victim.name.clone(), started);
        } else {
            outcome.failed.push(victim.name.clone());
        }
    }
    let confirmed = confirm_unloads(&outcome.requested, Duration::from_secs(5), || async {
        get_json(&state.client, &execution.upstream, "/api/ps")
            .await
            .ok()
    })
    .await;
    for (model, observed_at) in confirmed.confirmed {
        let victim = victims
            .iter()
            .find(|victim| victim.name == model)
            .expect("confirmed unload belongs to the eviction plan");
        let elapsed = observed_at.saturating_duration_since(requested_at[&model]);
        state.audit.record(&super::super::audit::Entry {
            backend: execution.placement.as_str(),
            model: &victim.name,
            event: "evict",
            trigger,
            duration_ms: Some(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
            bytes: Some(victim.bytes),
        });
        outcome.confirmed.push(model);
    }
    outcome.still_resident = confirmed.still_resident;
    outcome.observation_failed = confirmed.observation_failed;
    outcome.elapsed_ms = u64::try_from(unloading.elapsed().as_millis()).unwrap_or(u64::MAX);
    if !outcome.requested.is_empty() || !outcome.failed.is_empty() {
        // An error may still have delivered the unload. Invalidate before releasing lifecycle
        // claims, so arrivals cannot reuse a sample that predates this uncertain transition.
        state.resources.invalidate().await;
    }
    drop(claims);
    outcome
}

#[derive(Default)]
pub(super) struct UnloadConfirmation {
    pub(super) confirmed: Vec<(String, Instant)>,
    pub(super) still_resident: Vec<String>,
    pub(super) observation_failed: Vec<String>,
}

/// One `/api/ps` response observes every pending victim; the absolute deadline also bounds a
/// stalled observation. Malformed or unavailable residency is unknown, never evidence of absence.
pub(super) async fn confirm_unloads<F, Fut>(
    requested: &[String],
    wait: Duration,
    mut observe: F,
) -> UnloadConfirmation
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<Value>>,
{
    let deadline = tokio::time::Instant::now() + wait;
    let mut pending = requested.to_vec();
    let mut confirmation = UnloadConfirmation::default();
    let mut last_observation_valid = false;
    while !pending.is_empty() && tokio::time::Instant::now() < deadline {
        let sample = tokio::time::timeout_at(deadline, observe())
            .await
            .ok()
            .flatten();
        if let Some(ps) = sample.filter(|ps| {
            ps["models"].as_array().is_some_and(|models| {
                models
                    .iter()
                    .all(|entry| crate::ollama_http::ps_entry_name(entry).is_some())
            })
        }) {
            last_observation_valid = true;
            let observed_at = Instant::now();
            pending.retain(|model| {
                if crate::ollama_http::find_ps_entry(&ps, model).is_some() {
                    true
                } else {
                    confirmation.confirmed.push((model.clone(), observed_at));
                    false
                }
            });
        } else {
            last_observation_valid = false;
        }
        if !pending.is_empty() {
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + Duration::from_millis(200)).min(deadline),
            )
            .await;
        }
    }
    if last_observation_valid {
        confirmation.still_resident = pending;
    } else {
        confirmation.observation_failed = pending;
    }
    confirmation
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
    fn unrelated_or_aggregate_vram_cannot_discount_host_reservations() {
        for free in [0, 3000, 9000, u64::MAX] {
            let mut footprint = json!({"required_available_bytes": 8000});
            let observation = resources::HostResources {
                gpu_memory_free_bytes: Some(free),
                ..resources::HostResources::default()
            };
            apply_discrete_gpu_budget(&mut footprint, 8000, &observation);
            assert_eq!(footprint["required_available_bytes"], 8000);
            assert_eq!(footprint["gpu_capacity_credit_bytes"], 0);
            assert_eq!(footprint["gpu_memory_free_bytes"], free);
        }
    }

    #[tokio::test]
    async fn unload_confirmation_distinguishes_absence_from_a_still_resident_runner() {
        let requested = vec!["gone:latest".to_owned(), "busy:latest".to_owned()];
        let confirmation = confirm_unloads(&requested, Duration::from_millis(10), || async {
            Some(json!({"models":[{"name":"busy:latest"}]}))
        })
        .await;
        assert_eq!(confirmation.confirmed.len(), 1);
        assert_eq!(confirmation.confirmed[0].0, "gone:latest");
        assert_eq!(confirmation.still_resident, ["busy:latest"]);
        assert_eq!(confirmation.observation_failed, Vec::<String>::new());
    }

    #[tokio::test]
    async fn unavailable_or_malformed_residency_never_confirms_an_unload() {
        for sample in [
            None,
            Some(json!({"error":"unavailable"})),
            Some(json!({"models":[{}]})),
        ] {
            let confirmation =
                confirm_unloads(&["model:latest".into()], Duration::from_millis(10), || {
                    let sample = sample.clone();
                    async move { sample }
                })
                .await;
            assert_eq!(confirmation.confirmed, Vec::new());
            assert_eq!(confirmation.still_resident, Vec::<String>::new());
            assert_eq!(confirmation.observation_failed, ["model:latest"]);
        }
    }

    #[tokio::test]
    async fn unload_confirmation_deadline_bounds_a_stalled_observation() {
        let started = Instant::now();
        let confirmation =
            confirm_unloads(&["model:latest".into()], Duration::from_millis(10), || {
                std::future::pending::<Option<Value>>()
            })
            .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(confirmation.confirmed, Vec::new());
        assert_eq!(confirmation.observation_failed, ["model:latest"]);
    }
}
