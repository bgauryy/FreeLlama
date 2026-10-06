//! One managed task from route selection through admission, resources, transition, and forwarding.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{
    PlatformState, TaskInput, TaskKind, context, discover_models, host_has_unified_memory,
    require_active_session, resources,
};
use super::admit::admission_error;
use super::{
    AdmissionTimings, ForwardRequest, ManagedDecision, SLOT_HELD_RESOURCE_WAIT, TransitionPermit,
    admit, apply_execution_options, apply_request_options, build_managed_request,
    forward_managed_task, input_batch_items, make_room, memory_kv_preflight, model_is_resident,
    requests_immediate_unload, reserve_task_resources, resident_entry, select_managed_route,
    task_cost_for, task_has_images, transition_timeout,
};
use crate::model_bench::Capability;

#[expect(
    clippy::too_many_lines,
    reason = "explicit RAII lifetime across admission, revalidation and forwarding"
)]
pub(super) async fn run_session_task(
    State(state): State<PlatformState>,
    Json(mut input): Json<TaskInput>,
) -> Result<Json<Value>, ApiError> {
    let progress = input.job_progress.clone();
    // Function definitions are an execution requirement, not merely an optional payload field.
    // Derive the capability at the server boundary so direct HTTP/NAPI callers cannot accidentally
    // route tool work to a completion-only model.
    if input.tools.is_some() {
        input.route.required_capabilities.insert(Capability::Tools);
    }
    // Images likewise require a vision model on every surface (MCP, CLI, HTTP, NAPI).
    let has_images = task_has_images(&input);
    if has_images {
        input.route.required_capabilities.insert(Capability::Vision);
    }
    require_active_session(&state, input.route.session_id.as_deref()).await?;
    let batch_items = input_batch_items(&input);
    let requested_cost = task_cost_for(input.route.task, batch_items, &state.tunables());
    let models = discover_models(&state).await?;
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

    apply_request_options(input.route.task, &mut managed.route, &input.request_options)?;
    let mut context_sizing = context::size_context(&input, &mut managed.route, &managed.model)?;
    if managed.route.resident && context_sizing["mode"] == "prompt_estimate" {
        let loaded = resident_entry(
            &state.client,
            &managed.execution.upstream,
            &managed.route.selected_model,
        )
        .await;
        if let Some(reused) =
            context::reuse_resident_context(&mut managed.route, &managed.model, loaded.as_ref())
        {
            context_sizing["reused_resident_context"] = json!(reused);
        }
    }
    let leave_context_to_ollama =
        context_left_to_ollama(&state, &mut managed, &context_sizing).await;
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
    let _active = state
        .activity
        .begin(execution.placement.as_str(), &decision.selected_model);
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
    apply_execution_options(&mut body, &execution);
    if let Some(default_context) = leave_context_to_ollama {
        // Ollama's own default covers this request: leave `num_ctx` unset so the runner stays
        // in Ollama's automatic mode (shared with raw clients, OOM shrink-and-retry intact).
        if let Some(options) = body["options"].as_object_mut() {
            options.remove("num_ctx");
        }
        execution_receipt["context_sizing"]["num_ctx_sent"] = json!(false);
        execution_receipt["context_sizing"]["ollama_default_context"] = default_context;
    }
    execution_receipt["runtime_options"] = body["options"].clone();
    // Slot first, THEN the transition lock — in both branches. The order matters: if the
    // non-resident path took the write lock before its slot while resident tasks held slots and
    // waited on the read lock, the two would deadlock. One consistent order removes that entirely.
    let queue_wait = input.max_wait_seconds.map_or_else(
        || state.tunables().max_queue_wait(),
        |seconds| Duration::from_secs(seconds.max(1)).min(state.tunables().max_queue_wait()),
    );
    let queue_deadline = tokio::time::Instant::now() + queue_wait;
    if let Some(progress) = &progress {
        progress.update(
            super::super::jobs::JobStatus::WaitingForAdmission,
            "waiting_for_backend_capacity",
            None,
        );
    }
    let (mut slot, mut cost, mut queue_wait_ms) = admit(
        &state,
        &execution,
        requested_cost,
        input.priority,
        queue_deadline,
        None,
    )
    .await?;

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
    let (sized, evictions) = make_room(
        &state,
        &execution,
        &resource_model,
        &decision,
        queue_deadline,
    )
    .await;
    if let Some(progress) = &progress {
        let snapshot = state.resources.snapshot().await;
        progress.update(
            super::super::jobs::JobStatus::WaitingForResources,
            "waiting_for_safe_memory_and_pressure",
            Some(json!({
                "required_available_bytes": sized["required_available_bytes"],
                "effective_available_bytes": snapshot.effective_available_bytes,
                "reserve_bytes": snapshot.hold_reserve_bytes,
                "pressure_reasons": snapshot.reasons,
            })),
        );
    }
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
            Some(sized),
        ),
    )
    .await;
    let (resource_permit, mut footprint) = match reserved {
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
            ));
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
            let reserved = tokio::time::timeout_at(
                queue_deadline,
                reserve_task_resources(
                    &state,
                    &execution,
                    &resource_model,
                    &decision,
                    queue_deadline,
                    None,
                    None,
                ),
            )
            .await;
            let reserved = reserved
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "resource admission deadline exceeded",
                    )
                    .with_resource_deadline(
                        "initial_reservation",
                        resource_started.elapsed().as_millis(),
                    )
                })
                .and_then(std::convert::identity);
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
            let (readmitted, readmitted_cost, requeued_ms) = admit(
                &state,
                &execution,
                requested_cost,
                input.priority,
                queue_deadline,
                Some(resource_waiter),
            )
            .await?;
            slot = readmitted;
            cost = readmitted_cost;
            queue_wait_ms = queue_wait_ms.saturating_add(requeued_ms);
            reserved
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
    let mut resource_wait_ms = resource_started.elapsed().as_millis();
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
    let transition_started = Instant::now();
    let transition = tokio::time::timeout_at(queue_deadline, async {
        if decision.resident {
            let shared = Arc::clone(&execution.transition).read_owned().await;
            if model_is_resident(&state.client, &execution.upstream, &decision.selected_model).await
            {
                TransitionPermit::Shared(shared)
            } else {
                drop(shared);
                TransitionPermit::Exclusive(Arc::clone(&execution.transition).write_owned().await)
            }
        } else {
            TransitionPermit::Exclusive(Arc::clone(&execution.transition).write_owned().await)
        }
    })
    .await
    .map_err(|_| transition_timeout(&execution))?;
    let transition_wait_ms = transition_started.elapsed().as_millis();
    let recheck_started = Instant::now();
    let (resource_recheck, footprint) = tokio::time::timeout_at(
        queue_deadline,
        reserve_task_resources(
            &state,
            &execution,
            &resource_model,
            &decision,
            queue_deadline,
            Some(resource_permit.receipt.reserved_bytes),
            None,
        ),
    )
    .await
    .map_err(|_| {
        transition_timeout(&execution).with_resource_deadline(
            "reservation_revalidation",
            recheck_started.elapsed().as_millis(),
        )
    })??;
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
        progress.update(
            if admission_mode == "resident_shared" {
                super::super::jobs::JobStatus::Running
            } else {
                super::super::jobs::JobStatus::Loading
            },
            "forwarding_to_ollama",
            None,
        );
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
            immediate_unload,
            breaker_probe,
        },
    );
    // On unified memory a loaded runner's weights are wired and visible in OS telemetry, so the
    // reservation that covered the load is returned as soon as the runner is resident instead of
    // being counted a second time until the response ends. (With mmap'd weights on Linux the
    // page cache still reads as available, so there the reservation is held to the end.)
    let result = if progress.is_some()
        || host_has_unified_memory() && permits.iter().any(|permit| permit.reserved_bytes() > 0)
    {
        let client = state.client.clone();
        let upstream = execution.upstream.clone();
        let watch = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if model_is_resident(&client, &upstream, &selected_model).await {
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

/// Decide whether to leave `num_ctx` to Ollama for this request.
///
/// Only for prompt-sized contexts (an explicit `context_tokens` is always sent), and only when
/// Ollama's default for this model is known, covers the sized context, and does not exceed
/// `auto_context_max` (a 256k tier default would allocate a huge KV cache). A resident runner
/// whose context was reused keeps being addressed explicitly. On success the decision's
/// `num_ctx` is set to the default so memory estimates match what Ollama will allocate.
async fn context_left_to_ollama(
    state: &PlatformState,
    managed: &mut ManagedDecision,
    context_sizing: &Value,
) -> Option<Value> {
    let tunables = state.tunables();
    if tunables.context_mode != super::super::ContextMode::OllamaDefault
        || context_sizing["mode"] != "prompt_estimate"
        || context_sizing.get("reused_resident_context").is_some()
    {
        return None;
    }
    let sized = managed.route.options["num_ctx"].as_u64()?;
    let (default, source) = super::super::monitor::ollama_default_context(
        state,
        Some(&managed.model),
        managed.execution.placement,
    )
    .await?;
    let window = managed.model.advertised_context.unwrap_or(u64::MAX);
    if default < sized || default > tunables.auto_context_max || default > window {
        return None;
    }
    // A runner already loaded with some other explicit context would be reloaded by an
    // automatic request; keep addressing it explicitly (reuse handles the larger-window case).
    if managed.route.resident {
        let loaded = resident_entry(
            &state.client,
            &managed.execution.upstream,
            &managed.route.selected_model,
        )
        .await;
        if loaded
            .as_ref()
            .and_then(|entry| entry["context_length"].as_u64())
            .is_some_and(|loaded| loaded != default)
        {
            return None;
        }
    }
    managed.route.options["num_ctx"] = json!(default);
    managed
        .route
        .reasons
        .push("context_left_to_ollama_default".to_owned());
    Some(json!({"tokens": default, "source": source}))
}
