//! One managed task from route selection through admission, resources, transition, and forwarding.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use super::super::error::{ApiError, MemoryFigures};
use super::super::{
    PlatformState, TaskInput, TaskKind, host_has_unified_memory, require_active_session, resources,
};
use super::admit::admission_error;
use super::{
    AdmissionTimings, Forecast, ForwardRequest, SLOT_HELD_RESOURCE_WAIT, TransitionPermit, admit,
    apply_execution_options, build_managed_request, forward_managed_task, input_batch_items,
    make_room, memory_kv_preflight, requests_immediate_unload, reserve_task_resources,
    select_managed_route, task_cost_for, task_has_images, transition_timeout,
};

/// The caller's wait and the operator cap when the cap lowered it (`None` otherwise).
pub(super) fn wait_limit(
    requested_seconds: Option<u64>,
    cap: Duration,
    source: &str,
) -> Option<Value> {
    let requested = requested_seconds?;
    let cap_seconds = cap.as_secs_f64();
    (Duration::from_secs(requested.max(1)) > cap).then(|| {
        json!({
            "requested_seconds": requested,
            "effective_seconds": cap_seconds,
            "cap": "max_queue_wait_seconds",
            "cap_source": source,
            "note": format!(
                "waited at most {cap_seconds} s: the operator cap max_queue_wait_seconds ({source}; FREELLAMA_MAX_QUEUE_WAIT_SECONDS) lowered the requested {requested} s"
            ),
        })
    })
}

pub(super) async fn run_session_task(
    State(state): State<PlatformState>,
    Json(input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let limit = {
        let tunables = state.tunables();
        let source = tunables
            .sources
            .get("max_queue_wait_ms")
            .copied()
            .unwrap_or("default");
        wait_limit(input.max_wait_seconds, tunables.max_queue_wait(), source)
    };
    let route = input.route.clone();
    // Boxed: the pipeline future is large, and callers (nearest slices, jobs) hold this one.
    let outcome = match Box::pin(run_admitted_task(State(state.clone()), Json(input))).await {
        Err(error) => Err(refusal_guidance(&state, &route, error).await),
        ok => ok,
    };
    let Some(limit) = limit else {
        return outcome;
    };
    match outcome {
        Ok(Json(mut receipt)) => {
            if let Some(execution) = receipt.get_mut("execution").and_then(Value::as_object_mut) {
                execution.insert("wait_limit".into(), limit);
            }
            Ok(Json(receipt))
        }
        Err(error) if error.is_wait_deadline() => Err(error.with_wait_limit(limit)),
        Err(error) => Err(error),
    }
}

/// A memory refusal tells an agent what to do: the figures, who holds the memory, and which
/// installed models with the same task capability would fit the free memory now (or that none
/// does). Other errors pass through unchanged.
async fn refusal_guidance(
    state: &PlatformState,
    route: &super::super::RouteInput,
    error: ApiError,
) -> ApiError {
    if !error.is_resource_wait() || !error.memory_held() {
        return error;
    }
    let snapshot = state.resources.snapshot().await;
    let figures = error.assessed_memory().or_else(|| {
        error
            .deadline_required_bytes()
            .map(|required| MemoryFigures {
                required_with_reserve: required.saturating_add(snapshot.hold_reserve_bytes),
                reserve: snapshot.hold_reserve_bytes,
                free: snapshot.effective_available_bytes,
            })
    });
    let free = figures
        .and_then(|figures| figures.free)
        .or(snapshot.effective_available_bytes);
    let reserve = figures.map_or(snapshot.hold_reserve_bytes, |figures| figures.reserve);
    let alternatives = match (free, super::super::catalog::discover_models(state).await) {
        (Some(free), Ok(models)) => Some(fitting_models_hint(route, &models, free, reserve)),
        _ => None,
    };
    let holders = resources::memory_holders_now().await;
    error.with_memory_guidance(figures, holders, alternatives)
}

/// Installed models that satisfy the route's task capability and whose file size plus the
/// reserve fits `free`. File size is a lower bound (context memory comes on top), so the hint
/// says "by file size".
fn fitting_models_hint(
    route: &super::super::RouteInput,
    models: &[super::super::CatalogModel],
    free: u64,
    reserve: u64,
) -> String {
    let mut request = route.clone();
    request.model = None;
    let task = serde_json::to_value(request.task)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut fitting = super::super::routing::eligible_models(&request, models)
        .unwrap_or_default()
        .into_iter()
        .filter(|model| model.size.saturating_add(reserve) <= free)
        .collect::<Vec<_>>();
    fitting.sort_by_key(|model| std::cmp::Reverse(model.size));
    if fitting.is_empty() {
        return format!(
            "no installed `{task}` model fits {} free after the reserve; free memory (close the programs listed) or retry later",
            resources::gb(free)
        );
    }
    let names = fitting
        .iter()
        .take(5)
        .map(|model| format!("{} ({})", model.name, resources::gb(model.size)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "installed `{task}` models that fit {} free by file size (context memory is extra): {names}; pin one with `model`",
        resources::gb(free)
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "explicit RAII lifetime across admission, revalidation and forwarding"
)]
async fn run_admitted_task(
    State(state): State<PlatformState>,
    Json(mut input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let progress = input.job_progress.clone();
    let opportunistic = input.opportunistic_warm;
    // Refuse a task with nothing to do before routing loads a model for it.
    super::request::require_payload(&input)?;
    super::request::derive_input_capabilities(&mut input)?;
    let has_images = task_has_images(&input);
    require_active_session(&state, input.route.session_id.as_deref()).await?;
    let batch_items = input_batch_items(&input);
    let requested_cost = task_cost_for(input.route.task, batch_items, &state.tunables());
    let models =
        super::super::catalog::discover_for_route(&state, input.route.model.as_deref()).await?;
    let sessions = state.sessions.read().await;
    let mut managed = select_managed_route(
        &state,
        &input.route,
        &models,
        &sessions,
        requested_cost,
        Some(&input),
    )
    .await?;
    drop(sessions);
    // A `vision` capability tag is metadata, not evidence the build reads images (MLX conversions
    // have shipped without the vision head). Automatic image routing therefore needs an operator
    // policy naming the model for `vision`; otherwise the caller names a model it has trialled.
    if has_images
        && input.route.model.is_none()
        && !managed.model.policy_rank.contains_key(&TaskKind::Vision)
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "image input needs an explicit trialled vision `model`, or a serve policy file with `[policies.vision] qualified_models` (selected {} is not qualified for vision)",
            managed.route.selected_model
        )));
    }

    let (context_sizing, leave_context_to_ollama) =
        super::select::prepare_managed_context(&state, &input, &mut managed).await?;
    let preflight = memory_kv_preflight(
        &managed.model,
        &managed.route,
        managed.execution.placement,
        &managed.execution.upstream,
    );
    if preflight["refuses"] == true {
        return Err(ApiError::bad_request(
            "model file size alone exceeds the 80% local CPU/unified host budget; select a smaller model or configure an Ollama backend on a separate host",
        ));
    }

    let mut execution_receipt = managed.execution_receipt(requested_cost, &state).await;
    execution_receipt["context_sizing"] = context_sizing;
    execution_receipt["model_digest"] = json!(managed.model.digest);
    super::identity::verify_requested(&input, &managed, &mut execution_receipt)?;
    if let Some(progress) = &progress {
        progress.route(
            &managed.route.selected_model,
            managed.execution.placement.as_str(),
        );
    }
    let resource_model = managed.model;
    let decision = managed.route;
    let execution = managed.execution;
    // From here until the task ends this model counts as busy: eviction planning leaves it
    // alone, and its recent demand makes it more expensive to unload later.
    let queue_wait = input.max_wait_seconds.map_or_else(
        || state.tunables().max_queue_wait(),
        |seconds| Duration::from_secs(seconds.max(1)).min(state.tunables().max_queue_wait()),
    );
    let queue_deadline = tokio::time::Instant::now() + queue_wait;
    let active = if opportunistic {
        state
            .activity
            .try_begin_idle(execution.placement.as_str(), &decision.selected_model)
            .ok_or_else(|| opportunistic_skip("model lifecycle is busy"))?
    } else {
        state
            .activity
            .begin(execution.placement.as_str(), &decision.selected_model)
    };
    // Fail fast while this backend's circuit is open instead of queueing for a dead upstream.
    // The guard lets exactly one probe through after the cooldown.
    let breaker_probe = match state.breakers.check(execution.placement.as_str()) {
        Ok(probe) => probe,
        Err(remaining) => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "{} backend circuit open after repeated upstream failures; retry in {}s",
                    execution.placement,
                    remaining.as_secs().max(1)
                ),
            )
            .with_code("upstream_circuit_open")
            .with_retry_after(remaining.as_secs().max(1)));
        }
    };
    let immediate_unload = requests_immediate_unload(input.keep_alive.as_deref());
    // `keep_alive:0` makes Ollama unload before its response reaches FreeLlama, so `/api/ps`
    // cannot prove where the work ran. Hold the runner briefly, observe it, then issue and verify
    // an explicit unload before returning. The caller still receives immediate-unload semantics.
    let keep_alive = if immediate_unload {
        json!("30s")
    } else {
        let (value, receipt) = super::super::warming::keep_alive(
            &state,
            input.keep_alive.take(),
            execution.placement.as_str(),
            &decision.selected_model,
        )
        .await;
        execution_receipt["keep_alive"] = receipt;
        value
    };
    if immediate_unload {
        execution_receipt["keep_alive"] = json!({"mode":"explicit","value":0,"reason":"caller_immediate_unload_with_placement_observation","finite":true});
    }
    let (path, mut body) = build_managed_request(&mut input, &decision, &keep_alive)?;
    // `/v1/systemone` reads no options, so CPU placement there rests on the backend process.
    if path != "/v1/systemone" {
        apply_execution_options(&mut body, &execution);
    }
    if let Some(default_context) = leave_context_to_ollama {
        // Ollama's own default covers this request: leave `num_ctx` unset so the runner stays
        // in Ollama's automatic mode (shared with raw clients, OOM shrink-and-retry intact).
        if let Some(options) = body.get_mut("options").and_then(Value::as_object_mut) {
            options.remove("num_ctx");
        }
        execution_receipt["context_sizing"]["num_ctx_sent"] = json!(false);
        execution_receipt["context_sizing"]["ollama_default_context"] = default_context;
    }
    execution_receipt["runtime_options"] = body["options"].clone();
    // Slot first, THEN the transition lock — in both branches. The order matters: if the
    // non-resident path took the write lock before its slot while resident tasks held slots and
    // waited on the read lock, the two would deadlock. One consistent order removes that entirely.
    if let Some(progress) = &progress {
        progress.update(
            super::super::jobs::JobStatus::WaitingForAdmission,
            "waiting_for_backend_capacity",
            None,
        );
    }
    let (mut slot, mut cost, mut queue_wait_ms) = if opportunistic {
        let permit = execution
            .admission
            .try_acquire_idle(requested_cost as usize)
            .ok_or_else(|| opportunistic_skip("backend has active or queued work"))?;
        let cost = u32::try_from(permit.units()).unwrap_or(u32::MAX);
        (permit, cost, 0)
    } else {
        admit(
            &state,
            &execution,
            requested_cost,
            input.priority,
            queue_deadline,
            None,
        )
        .await?
    };
    // Demand registers before admission to protect queued models, but waiting for an existing
    // unload starts only after the bounded queue grants a slot. There is no extra waiter pool.
    let lifecycle_started = Instant::now();
    tokio::time::timeout_at(queue_deadline, active.ready())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "model unload is still pending; retry the task",
            )
            .with_code("model_lifecycle_busy")
            .with_retry_after(1)
        })?;
    execution_receipt["lifecycle_wait_ms"] =
        json!(u64::try_from(lifecycle_started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let early_transition = if opportunistic {
        Some(TransitionPermit::Loading(
            Arc::clone(&execution.transition)
                .try_lock_owned()
                .map_err(|_| opportunistic_skip("backend transition is busy"))?,
        ))
    } else {
        None
    };

    // Wait for host memory while holding the admission slot only briefly, so a cold load does
    // not block resident requests that need no new memory (head-of-line blocking). After the
    // short bound it gives the slot back, waits for memory, then queues for a slot again with
    // memory reserved.
    if let Some(progress) = &progress {
        progress.update(
            super::super::jobs::JobStatus::WaitingForResources,
            "checking_host_resources",
            None,
        );
    }
    let resource_started = Instant::now();
    let (sized, evictions) = if opportunistic {
        (
            super::memory::model_memory_requirement(
                &state,
                &execution,
                &resource_model,
                super::memory::requested_context(&decision),
            )
            .await,
            Vec::new(),
        )
    } else {
        make_room(
            &state,
            &execution,
            &resource_model,
            super::memory::requested_context(&decision),
            queue_deadline,
        )
        .await
    };
    let mut readmission_elapsed = Duration::ZERO;
    let sized_bytes = sized["required_available_bytes"].as_u64();
    if let Some(progress) = &progress {
        let snapshot = state.resources.snapshot().await;
        let memory_holders = if snapshot.reasons.iter().any(|reason| reason.is_memory()) {
            Some(resources::memory_holders_now().await).filter(|holders| !holders.is_empty())
        } else {
            None
        };
        progress.update(
            super::super::jobs::JobStatus::WaitingForResources,
            "waiting_for_safe_memory_and_pressure",
            Some(json!({
                "required_available_bytes": sized["required_available_bytes"],
                "effective_available_bytes": snapshot.effective_available_bytes,
                "reserve_bytes": snapshot.hold_reserve_bytes,
                "pressure_reasons": snapshot.reasons,
                // Who holds the memory, while a memory signal holds the host: what to close.
                "memory_holders": memory_holders,
            })),
        );
    }
    let (resource_permit, mut footprint) = if opportunistic {
        reserve_opportunistic(&state, &execution, &decision.selected_model, sized, None).await?
    } else {
        let quick_deadline =
            (tokio::time::Instant::now() + SLOT_HELD_RESOURCE_WAIT).min(queue_deadline);
        let reserved = tokio::time::timeout_at(
            quick_deadline,
            reserve_task_resources(
                &state,
                &execution,
                &resource_model,
                &decision,
                quick_deadline,
                None,
                Forecast::Sized(sized),
            ),
        )
        .await;
        match reserved {
            Ok(Ok(reserved)) => reserved,
            Ok(Err(error)) if !error.is_resource_wait() => return Err(error),
            _ if tokio::time::Instant::now() >= queue_deadline => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "resource admission deadline exceeded",
                )
                .with_resource_deadline(
                    "initial_reservation",
                    resource_started.elapsed().as_millis(),
                    sized_bytes,
                )
                .with_holds(state.resources.snapshot().await.reasons));
            }
            _ => {
                let resource_waiter =
                    execution
                        .admission
                        .register_resource_waiter()
                        .map_err(|failure| {
                            admission_error(&state, &execution, input.priority, cost, failure)
                        })?;
                drop(slot);
                let reserved = reserve_while_models_drain(
                    &state,
                    &execution,
                    &resource_model,
                    &decision,
                    queue_deadline,
                )
                .await;
                let reserved = match reserved {
                    Ok(reserved) => reserved,
                    Err(_) => Err(ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "resource admission deadline exceeded",
                    )
                    .with_resource_deadline(
                        "initial_reservation",
                        resource_started.elapsed().as_millis(),
                        sized_bytes,
                    )
                    .with_holds(state.resources.snapshot().await.reasons)),
                };
                let reserved = match reserved {
                    Ok(reserved) => reserved,
                    Err(error) => {
                        resource_waiter.finish();
                        return Err(error);
                    }
                };
                if let Some(progress) = &progress {
                    progress.update(
                        super::super::jobs::JobStatus::WaitingForAdmission,
                        "resources_reserved_waiting_for_backend_capacity",
                        None,
                    );
                }
                let readmission_started = Instant::now();
                let (readmitted, readmitted_cost, requeued_ms) = admit(
                    &state,
                    &execution,
                    requested_cost,
                    input.priority,
                    queue_deadline,
                    Some(resource_waiter),
                )
                .await?;
                readmission_elapsed = readmission_started.elapsed();
                slot = readmitted;
                cost = readmitted_cost;
                queue_wait_ms = queue_wait_ms.saturating_add(requeued_ms);
                reserved
            }
        }
    };
    if !evictions.is_empty() {
        footprint["evicted_idle_models"] = json!(
            evictions
                .iter()
                .flat_map(|receipt| receipt["unloaded_ok"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default())
                .collect::<Vec<_>>()
        );
        footprint["eviction"] = json!(evictions);
    }
    // Re-admission is already counted in queue_wait_ms. Its wall interval must not also
    // inflate the resource phase and therefore total_wait_ms.
    let mut resource_wait_ms = resource_started
        .elapsed()
        .saturating_sub(readmission_elapsed)
        .as_millis();
    execution_receipt["resource_admission"] = json!(resource_permit.receipt);
    execution_receipt["memory_reservation"] = footprint;

    // Residency was discovered before admission and can be stale by the time this request reaches
    // the transition lock. Recheck while holding the read side: if the selected runner is still
    // resident, that lock prevents a managed writer from transitioning it during execution. A
    // stale or unavailable snapshot falls back to the exclusive side before the request is sent.
    if let Some(progress) = &progress {
        progress.update(
            super::super::jobs::JobStatus::WaitingForRunner,
            "waiting_for_runner_transition",
            None,
        );
    }
    let mut expectation = super::runner::RunnerExpectation::new(&resource_model, &decision)
        .with_runtime_options(&body["options"]);
    if matches!(
        input.route.min_placement_evidence,
        super::super::PlacementEvidence::Observed
    ) {
        expectation = expectation.requiring_gpu_placement(execution.placement);
    }
    let transition_started = Instant::now();
    let (transition, before, observed_ps) = tokio::time::timeout_at(queue_deadline, async {
        if let Some(transition) = early_transition {
            let ps = super::super::get_json(&state.client, &execution.upstream, "/api/ps")
                .await
                .ok();
            let receipt = expectation.receipt(ps.as_ref(), &decision.selected_model);
            return (transition, receipt, ps);
        }
        // A compatible resident runner needs no backend lock: this task's model use lease
        // already keeps it from being evicted, so it never waits for another model's load.
        let ps = super::super::get_json(&state.client, &execution.upstream, "/api/ps")
            .await
            .ok();
        let observed = expectation.receipt(ps.as_ref(), &decision.selected_model);
        if observed["compatible"] == true {
            return (TransitionPermit::Resident, observed, ps);
        }
        // Loads and reloads queue on the backend load lock. Observe again once it is won: a
        // task for this same model that held the lock first has made the runner resident, and
        // this one then shares it instead of loading again.
        let load = Arc::clone(&execution.transition).lock_owned().await;
        let ps = super::super::get_json(&state.client, &execution.upstream, "/api/ps")
            .await
            .ok();
        let observed = expectation.receipt(ps.as_ref(), &decision.selected_model);
        if observed["compatible"] == true {
            drop(load);
            (TransitionPermit::Resident, observed, ps)
        } else {
            (TransitionPermit::Loading(load), observed, ps)
        }
    })
    .await
    .map_err(|_| transition_timeout(&execution))?;
    execution_receipt["runner_reuse"] = before;
    let transition_wait_ms = transition_started.elapsed().as_millis();
    let recheck_started = Instant::now();
    let (resource_recheck, footprint) = if opportunistic {
        let sized = super::memory::model_memory_requirement(
            &state,
            &execution,
            &resource_model,
            super::memory::requested_context(&decision),
        )
        .await;
        reserve_opportunistic(
            &state,
            &execution,
            &decision.selected_model,
            sized,
            Some(resource_permit.receipt.reserved_bytes),
        )
        .await?
    } else {
        let revalidated = tokio::time::timeout_at(
            queue_deadline,
            reserve_task_resources(
                &state,
                &execution,
                &resource_model,
                &decision,
                queue_deadline,
                Some(resource_permit.receipt.reserved_bytes),
                // The listing that decided the transition is moments old: forecast from it.
                observed_ps
                    .as_ref()
                    .map_or(Forecast::Measure, Forecast::Observed),
            ),
        )
        .await;
        match revalidated {
            Ok(revalidated) => revalidated?,
            Err(_) => {
                return Err(transition_timeout(&execution)
                    .with_resource_deadline(
                        "reservation_revalidation",
                        recheck_started.elapsed().as_millis(),
                        sized_bytes,
                    )
                    .with_holds(state.resources.snapshot().await.reasons));
            }
        }
    };
    resource_wait_ms = resource_wait_ms.saturating_add(recheck_started.elapsed().as_millis());
    execution_receipt["resource_revalidation"] = json!(resource_recheck.receipt);
    let previous = execution_receipt["memory_reservation"].clone();
    execution_receipt["memory_reservation"] = footprint;
    for key in ["evicted_idle_models", "eviction"] {
        if let Some(value) = previous.get(key) {
            execution_receipt["memory_reservation"][key] = value.clone();
        }
    }
    let admission_mode = transition.admission_mode();
    let selected_model = decision.selected_model.clone();
    let session_id = input.route.session_id.clone();
    let mut permits = [resource_permit, resource_recheck];
    if let Some(progress) = &progress {
        if admission_mode == "resident_shared" {
            progress.update(
                super::super::jobs::JobStatus::Running,
                "forwarding_to_ollama",
                None,
            );
        } else {
            // What a waiting caller needs to say "loading X (~N s)": the model, where it loads,
            // the last measured load time (absent when never measured) and what made room.
            let usage = state
                .activity
                .usage(execution.placement.as_str(), &selected_model);
            progress.update(
                super::super::jobs::JobStatus::Loading,
                "forwarding_to_ollama",
                Some(json!({
                    "model": selected_model,
                    "backend": execution.placement.as_str(),
                    "expected_load_ms": usage.load_ms.map(f64::round),
                    "evicted": execution_receipt["memory_reservation"]["evicted_idle_models"]
                        .clone(),
                })),
            );
        }
    }
    if opportunistic
        && (execution.admission.queue_depth() > 0
            || execution.admission.receipt()["in_flight"] != 1)
    {
        return Err(opportunistic_skip(
            "foreground demand arrived before dispatch",
        ));
    }
    let forward = forward_managed_task(
        &state,
        ForwardRequest {
            decision,
            execution: &execution,
            execution_receipt,
            path,
            body,
            admission: AdmissionTimings {
                mode: admission_mode,
                queue_wait_ms,
                transition_wait_ms,
                resource_wait_ms,
                cost,
            },
            slot,
            breaker_probe,
        },
    );
    // On unified memory a loaded runner's weights are wired and visible in OS telemetry, so the
    // reservation that covered the load is returned as soon as the runner is resident instead of
    // being counted a second time until the response ends. (With mmap'd weights on Linux the
    // page cache still reads as available, so there the reservation is held to the end.)
    // A cold load owns the backend load lock. Once the runner is observed resident the
    // transition is over and the lock is released, so the next load proceeds while this task
    // generates. This runner cannot be evicted meanwhile because its task holds the model's
    // use lease (`active`).
    let mut transition = Some(transition);
    let loading = matches!(transition, Some(TransitionPermit::Loading(_)));
    let mut result = if progress.is_some()
        || loading
        || host_has_unified_memory() && permits.iter().any(|permit| permit.reserved_bytes() > 0)
    {
        let client = state.client.clone();
        let upstream = execution.upstream.clone();
        let watch = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let loaded = super::super::get_json(&client, &upstream, "/api/ps")
                    .await
                    .ok();
                if loaded
                    .as_ref()
                    .is_some_and(|ps| expectation.matches_in(ps, &selected_model))
                {
                    runner_ready(&mut transition);
                    if let Some(progress) = &progress {
                        progress.update(
                            super::super::jobs::JobStatus::Running,
                            "resident_runner_observed",
                            None,
                        );
                    }
                    if host_has_unified_memory() {
                        permits
                            .iter_mut()
                            .for_each(resources::ResourcePermit::release);
                    }
                    if loading {
                        state.activity.runner_ready();
                    }
                    break;
                }
            }
            std::future::pending::<()>().await;
        };
        tokio::select! {
            result = forward => result,
            () = watch => unreachable!("the residency watch never completes"),
        }
    } else {
        forward.await
    };
    drop(transition);
    drop(permits);
    drop(active);
    if immediate_unload {
        let lifecycle = post_task_unload(&state, &selected_model, queue_wait).await;
        result = result
            .map(|Json(mut value)| {
                value["execution"]["lifecycle"] = lifecycle.clone();
                Json(value)
            })
            .map_err(|error| error.with_lifecycle(lifecycle));
    }

    // Affinity means the last model that successfully executed for the session. An upstream error
    // must not pin a model the caller never received a successful result from.
    if result.is_ok()
        && input.scope_id.is_none()
        && let Some(id) = session_id.as_deref()
    {
        state.sessions.write().await.bind(id, &selected_model);
    }
    result
}

fn opportunistic_skip(reason: &str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, reason).with_code("opportunistic_warm_skipped")
}

/// Observing readiness ends a transition, not the generation: the load lock is released while
/// the task keeps generating under its model use lease. A resident task has no lock to release
/// (its watch runs only to update deferred-job progress).
fn runner_ready(transition: &mut Option<TransitionPermit>) {
    if matches!(transition, Some(TransitionPermit::Loading(_))) {
        *transition = Some(TransitionPermit::Resident);
    }
}

/// Wait for memory the load needs. A model that was busy when eviction was planned can become
/// idle while this task waits; when a use ends, plan eviction again. Runner readiness also wakes
/// this wait so a same-model follower re-sizes as resident instead of reserving another copy.
async fn reserve_while_models_drain(
    state: &PlatformState,
    execution: &super::ExecutionTarget,
    model: &super::super::CatalogModel,
    decision: &super::super::RouteDecision,
    deadline: tokio::time::Instant,
) -> Result<Result<(resources::ResourcePermit, Value), ApiError>, tokio::time::error::Elapsed> {
    // Register before the first plan: a model whose use ended while this task was queued or
    // sizing (before this wait began) emits no further wake, so it is planned for right away.
    let mut changed = Box::pin(state.activity.changed());
    make_room(
        state,
        execution,
        model,
        super::memory::requested_context(decision),
        deadline,
    )
    .await;
    loop {
        let reserve = tokio::time::timeout_at(
            deadline,
            reserve_task_resources(
                state,
                execution,
                model,
                decision,
                deadline,
                None,
                Forecast::Measure,
            ),
        );
        tokio::select! {
            reserved = reserve => return reserved,
            () = &mut changed => {
                // Register before planning, so a use that ends meanwhile is not missed.
                changed = Box::pin(state.activity.changed());
                make_room(state, execution, model, super::memory::requested_context(decision), deadline).await;
            }
        }
    }
}

async fn post_task_unload(state: &PlatformState, model: &str, wait: Duration) -> Value {
    match super::unload_resident_model(state, model, wait).await {
        Ok(receipt) => {
            let unloaded = receipt
                .as_ref()
                .is_none_or(|receipt| receipt["unloaded"] == true);
            let status = receipt
                .as_ref()
                .and_then(|receipt| receipt["status"].as_str())
                .unwrap_or("not_resident");
            json!({
                "requested":"immediate_unload", "status":if unloaded {"verified"} else {"failed"},
                "post_unload_observation": {
                    "resident": if unloaded {json!(false)} else if status == "still_resident" {json!(true)} else {Value::Null},
                    "status":if unloaded {"not_resident"} else {status},
                    "source":"ollama_api_ps_after_unload",
                },
                "unload":receipt,
            })
        }
        Err(error) => {
            json!({"requested":"immediate_unload", "status":"failed", "error":error.body.error,
            "post_unload_observation":{"resident":null,"status":"unavailable"}})
        }
    }
}

/// A prediction must fit without waiting or displacing an existing runner. Check again while
/// holding the transition writer; the governor atomically refuses unsafe/stale host capacity.
async fn reserve_opportunistic(
    state: &PlatformState,
    execution: &super::ExecutionTarget,
    model: &str,
    footprint: Value,
    already_reserved: Option<u64>,
) -> Result<(resources::ResourcePermit, Value), ApiError> {
    let ps = super::super::get_json(&state.client, &execution.upstream, "/api/ps")
        .await
        .map_err(|_| opportunistic_skip("runner inventory unavailable"))?;
    let running = ps["models"]
        .as_array()
        .ok_or_else(|| opportunistic_skip("runner inventory unavailable"))?;
    if crate::ollama_http::find_ps_entry(&ps, model).is_some() {
        return Err(opportunistic_skip("model is already resident"));
    }
    if !running.is_empty() {
        let settings = super::select::comparable_backend_settings(state, execution).await;
        let limit = settings
            .as_ref()
            .and_then(|settings| settings["settings"]["OLLAMA_MAX_LOADED_MODELS"]["value"].as_str())
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|limit| *limit > 0);
        if limit.is_none_or(|limit| running.len() >= limit) {
            return Err(opportunistic_skip(
                "Ollama runner-count headroom is not verified",
            ));
        }
    }
    let load = footprint["required_available_bytes"]
        .as_u64()
        .ok_or_else(|| opportunistic_skip("model memory requirement is unknown"))?;
    if super::memory::host_memory_relevant(&execution.upstream, execution.placement) {
        if let Some(budget) = state
            .tunables()
            .model_memory_budget
            .bytes(super::super::host_total_memory_bytes())
            && super::super::budget::overage(
                budget,
                super::super::budget::resident_bytes(&ps, model),
                load,
            ) > 0
        {
            return Err(opportunistic_skip(
                "model memory budget would require eviction",
            ));
        }
    } else {
        // A discrete GPU (or a remote backend): vendor-wide free VRAM does not say which devices
        // this Ollama process may allocate on, and nothing in this process verifies that
        // (`gpu_capacity_scope` stays `host_aggregate_not_verified_for_backend`). A speculative
        // load must never displace work on an unverified pool, so predictive warm refuses here.
        // Explicit warms and managed tasks still run through ordinary admission.
        return Err(opportunistic_skip(
            "GPU device eligibility for this Ollama backend is unverified",
        ));
    }
    let demand =
        resources::ResourceDemand::load(load.saturating_sub(already_reserved.unwrap_or(0)))
            .on_cpu(execution.placement == super::super::Backend::Cpu);
    let permit = state
        .resources
        .try_reserve_idle(&execution.upstream, demand)
        .await
        .map_err(|_| opportunistic_skip("safe idle host capacity unavailable"))?;
    Ok((permit, footprint))
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn a_resident_task_holds_no_backend_lock_while_its_watch_runs() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut transition = Some(TransitionPermit::Resident);
        // A deferred resident request runs the same watch as a cold request. Readiness must
        // not conjure or release anything: another model can load at any time.
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn a_cold_watch_releases_the_load_lock_when_the_runner_is_ready() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut transition = Some(TransitionPermit::Loading(
            Arc::clone(&lock).lock_owned().await,
        ));
        assert!(lock.try_lock().is_err());
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
        assert!(lock.try_lock().is_ok(), "the next load may start");
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
    }

    #[tokio::test]
    async fn a_cancelled_loader_releases_the_load_lock() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let transition = Some(TransitionPermit::Loading(
            Arc::clone(&lock).lock_owned().await,
        ));
        assert!(lock.try_lock().is_err());
        drop(transition); // The request was cancelled before its runner became ready.
        assert!(lock.try_lock().is_ok());
    }
}

#[cfg(test)]
mod wait_limit_tests {
    use super::{Duration, wait_limit};

    #[test]
    fn a_wait_lowered_by_the_operator_cap_is_reported_and_others_are_not() {
        let cap = Duration::from_secs(120);
        let limit = wait_limit(Some(1800), cap, "default").expect("1800 s exceeds the cap");
        assert_eq!(limit["requested_seconds"], 1800);
        assert_eq!(limit["effective_seconds"], 120.0);
        assert_eq!(limit["cap_source"], "default");
        assert!(
            limit["note"]
                .as_str()
                .unwrap()
                .contains("lowered the requested 1800 s")
        );
        assert!(wait_limit(Some(120), cap, "default").is_none());
        assert!(wait_limit(Some(30), cap, "env").is_none());
        assert!(wait_limit(None, cap, "default").is_none());
    }
}

#[cfg(test)]
mod memory_refusal_tests {
    use super::fitting_models_hint;
    use crate::platform::{CatalogModel, RouteInput};
    use serde_json::json;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn model(name: &str, capability: &str, size: u64) -> CatalogModel {
        serde_json::from_value(json!({
            "name":name,"digest":name,"size":size,"capabilities":[capability],
            "advertised_context":32768,"resident":false,"benchmark":{},"policy_rank":{}
        }))
        .unwrap()
    }

    fn route(task: &str) -> RouteInput {
        serde_json::from_value(json!({"task": task})).unwrap()
    }

    #[test]
    fn the_hint_lists_same_capability_models_that_fit_and_skips_the_rest() {
        let models = [
            model("big-decider", "decision", 9 * GIB),
            model("small-decider", "decision", GIB),
            model("chat", "completion", GIB),
        ];
        let hint = fitting_models_hint(&route("decision"), &models, 4 * GIB, GIB);
        assert!(hint.contains("`decision`"), "{hint}");
        assert!(hint.contains("small-decider (1.0 GB)"), "{hint}");
        assert!(!hint.contains("big-decider"), "{hint}");
        assert!(!hint.contains("chat"), "{hint}");
    }

    #[test]
    fn the_hint_says_none_fits_when_nothing_does() {
        let models = [model("big-decider", "decision", 9 * GIB)];
        let hint = fitting_models_hint(&route("decision"), &models, 4 * GIB, GIB);
        assert!(
            hint.starts_with("no installed `decision` model fits 4.0 GB free"),
            "{hint}"
        );
    }
}
