//! Forwarding a managed task to Ollama: completion validation, placement observation and outcome recording.

use anyhow::Result;
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

use super::super::admission::AdmissionPermit;
use super::super::error::ApiError;
use super::super::{
    Backend, FEEDBACK_SCHEMA_VERSION, PlatformState, RouteDecision, persist_feedback,
};
use super::ExecutionTarget;
use super::feedback::{adaptive_throughput_metric, observe_completion};
use super::feedback::{feedback_work_unit_ns, runtime_metrics};
use super::observe::{observe_physical_placement, unload_after_observation};
use super::select::{
    ProfileEvidence, comparable_backend_settings, profile_from_observations,
    unchanged_admission_interval,
};
use super::upstream::{post_json_with_retries, post_streaming_chat_with_retries};

/// Feed the circuit breaker with one upstream call's outcome.
fn record_upstream_outcome(
    state: &PlatformState,
    execution: &ExecutionTarget,
    probe: Option<super::super::runtime::BreakerProbe>,
    failed: bool,
) {
    let tunables = state.tunables();
    drop(probe);
    if state.breakers.record(
        execution.placement.as_str(),
        !failed,
        tunables.breaker_failures,
        tunables.breaker_cooldown(),
    ) {
        state
            .telemetry
            .record_breaker_open(execution.placement.as_str());
        eprintln!(
            "circuit breaker: {} backend open for {}s after repeated upstream failures",
            execution.placement, tunables.breaker_cooldown_seconds
        );
    }
}

async fn validate_upstream_completion(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
    path: &str,
    status: StatusCode,
    value: Value,
    immediate_unload: bool,
) -> Result<Value, ApiError> {
    let reported_error = value.get("error").is_some_and(|error| !error.is_null());
    let incomplete =
        status.is_success() && !reported_error && path == "/api/chat" && value["done"] != true;
    if !status.is_success() || incomplete || reported_error {
        // A completed HTTP exchange can still contain an incomplete generation or an error.
        // Do not train feedback or bind affinity. Honor this request's immediate-unload contract,
        // but never replay the uncertain generation to manufacture a completed response.
        let lifecycle = if immediate_unload {
            Some(unload_after_observation(state, execution, model).await)
        } else {
            None
        };
        let code = if incomplete {
            "upstream_incomplete_response"
        } else {
            "upstream_error"
        };
        let message = if incomplete {
            "Ollama did not return done:true for a managed request".to_owned()
        } else {
            value["error"]
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        };
        return Err(ApiError::new(
            if status.is_success() {
                StatusCode::BAD_GATEWAY
            } else {
                status
            },
            message,
        )
        .with_upstream_response(code, value, lifecycle));
    }
    Ok(value)
}

/// How a managed task was admitted, reported in its receipt.
pub(crate) struct AdmissionTimings<'a> {
    pub(crate) mode: &'a str,
    pub(crate) queue_wait_ms: u128,
    pub(crate) transition_wait_ms: u128,
    pub(crate) resource_wait_ms: u128,
    pub(crate) cost: u32,
}

/// One admitted managed request ready to forward upstream.
pub(crate) struct ForwardRequest<'a> {
    pub(crate) decision: RouteDecision,
    pub(crate) execution: &'a ExecutionTarget,
    pub(crate) execution_receipt: Value,
    pub(crate) path: &'a str,
    pub(crate) body: Value,
    pub(crate) admission: AdmissionTimings<'a>,
    /// Remains owned until completion, including validation and requested unload.
    pub(crate) slot: AdmissionPermit,
    pub(crate) immediate_unload: bool,
    pub(crate) breaker_probe: Option<super::super::runtime::BreakerProbe>,
}

#[expect(
    clippy::too_many_lines,
    reason = "one streaming forward loop with its terminal-state bookkeeping"
)]
pub(crate) async fn forward_managed_task(
    state: &PlatformState,
    request: ForwardRequest<'_>,
) -> Result<Json<Value>, ApiError> {
    let ForwardRequest {
        decision,
        execution,
        mut execution_receipt,
        path,
        body,
        admission:
            AdmissionTimings {
                mode: admission_mode,
                queue_wait_ms,
                transition_wait_ms,
                resource_wait_ms,
                cost,
            },
        slot,
        immediate_unload,
        breaker_probe,
    } = request;
    let mut completion_guard = super::super::monitor::adaptive_applies(state, execution.placement)
        .then(|| state.adaptive_for(execution.placement).completion_guard());
    let initial_admission = execution.admission.receipt();
    let dispatch_settings =
        if admission_mode == "resident_shared" && initial_admission["in_flight"] == 1 {
            comparable_backend_settings(state, execution).await
        } else {
            None
        };
    let dispatch_admission = execution.admission.receipt();
    let posted = if body["stream"] == true {
        post_streaming_chat_with_retries(state, &execution.upstream, path, &body).await
    } else {
        post_json_with_retries(state, &execution.upstream, path, &body).await
    };
    let upstream_failed = match &posted {
        Err(_) => true,
        Ok((status, _)) => matches!(status.as_u16(), 500 | 502 | 504),
    };
    record_upstream_outcome(state, execution, breaker_probe, upstream_failed);
    if upstream_failed {
        observe_completion(state, execution, true, None, None).await;
    }
    let (status, value) = match posted {
        Ok(response) => response,
        Err(error) => {
            // The runner may have loaded before the response or its body failed. We replaced
            // the caller's zero TTL with 30s to observe placement, so finish the owned cleanup
            // even when no response can be decoded. Never replay uncertain inference.
            return Err(if immediate_unload {
                error.with_lifecycle(
                    unload_after_observation(state, execution, &decision.selected_model).await,
                )
            } else {
                error
            });
        }
    };
    let value = validate_upstream_completion(
        state,
        execution,
        &decision.selected_model,
        path,
        status,
        value,
        immediate_unload,
    )
    .await?;
    let metrics = runtime_metrics(&value);
    let placement = observe_physical_placement(state, execution, &decision.selected_model).await;
    let feedback_accepted = placement["status"] == "verified";
    let observed_admission = execution.admission.receipt();
    let completion_settings = if dispatch_settings.is_some()
        && unchanged_admission_interval(&dispatch_admission, &observed_admission)
    {
        comparable_backend_settings(state, execution).await
    } else {
        None
    };
    let completion_admission = execution.admission.receipt();
    let learning_profile = profile_from_observations(ProfileEvidence {
        task: decision.task,
        model: &decision.selected_model,
        catalog_digest: execution_receipt["model_digest"].as_str(),
        upstream: &execution.upstream,
        body: &body,
        placement: &placement,
        dispatch_admission: &dispatch_admission,
        completion_admission: &completion_admission,
        dispatch_settings: dispatch_settings.as_ref(),
        completion_settings: completion_settings.as_ref(),
        warm: admission_mode == "resident_shared",
    });
    let diagnostic_work_score = if admission_mode == "resident_shared" {
        feedback_work_unit_ns(decision.task, &value)
    } else {
        None
    };
    let work_score = learning_profile.as_ref().ok().and(diagnostic_work_score);
    let adaptive_metric = adaptive_throughput_metric(decision.task, &value);
    execution_receipt["throughput_learning"] = json!({
        "eligible": work_score.is_some(),
        "profile_eligible": learning_profile.is_ok(),
        "adaptive_eligible": learning_profile.is_ok() && adaptive_metric.is_some(),
        "reason": learning_profile.as_ref().err().copied().unwrap_or(if work_score.is_some() {"comparable_observed_profile"} else {"work_unit_metrics_unknown"}),
        "profile": learning_profile.as_ref().ok(),
        "metric_kind": adaptive_metric.map(|metric| metric.kind),
        "metric_value": adaptive_metric.map(|metric| metric.tokens_per_second),
        "scope": "observed_controls_and_serial_freellama_admission",
    });
    observe_completion(
        state,
        execution,
        false,
        learning_profile.as_ref().ok(),
        adaptive_metric,
    )
    .await;
    if let Some(guard) = &mut completion_guard {
        guard.complete();
    }
    let save = state.footprints.lock().await.observe(
        &execution.upstream,
        execution_receipt["model_digest"].as_str(),
        &placement,
    );
    if let Some(save) = save {
        save.persist().await;
    }
    execution_receipt["observation"] = placement;
    let slots_total = execution.admission.total();
    // Report throttling rather than hiding it. A caller that fans out embeddings needs to know it
    // is queueing here — otherwise the only symptom is latency it cannot attribute.
    let slots_available = execution.admission.available();
    let feedback_receipt = {
        let mut feedback = state.feedback.write().await;
        let by_task = if execution.placement == Backend::Cpu {
            &mut feedback.cpu
        } else {
            &mut feedback.gpu
        };
        let observation = by_task.entry(decision.task).or_default();
        if feedback_accepted {
            observation.record(
                &decision.selected_model,
                diagnostic_work_score,
                queue_wait_ms,
            );
            if let (Ok(profile), Some(duration)) = (&learning_profile, work_score) {
                observation.record_profile(profile, duration);
            }
        }
        let mut receipt = observation.receipt();
        receipt["accepted"] = json!(feedback_accepted);
        receipt["throughput_learning"] = execution_receipt["throughput_learning"].clone();
        receipt["reason"] = json!(if feedback_accepted {
            "physical_placement_verified"
        } else {
            "physical_placement_unverified_or_mismatched"
        });
        let snapshot = feedback.clone();
        drop(feedback);
        let persistence = if feedback_accepted {
            if let Some(path) = state.feedback_file.clone() {
                // A synchronous fsync-and-rename on a runtime worker stalls every task sharing it.
                let written =
                    tokio::task::spawn_blocking(move || persist_feedback(&path, &snapshot))
                        .await
                        .unwrap_or_else(|error| {
                            Err(anyhow::anyhow!("feedback writer panicked: {error}"))
                        });
                match written {
                    Ok(()) => {
                        *state.feedback_persistence_error.write().await = None;
                        json!({"enabled": true, "persisted": true, "schema_version": FEEDBACK_SCHEMA_VERSION})
                    }
                    Err(error) => {
                        let message = error.to_string();
                        *state.feedback_persistence_error.write().await = Some(message.clone());
                        json!({"enabled": true, "persisted": false, "error": message})
                    }
                }
            } else {
                json!({"enabled": false, "persisted": false})
            }
        } else {
            json!({"enabled": state.feedback_file.is_some(), "persisted": false, "reason": "sample_not_accepted"})
        };
        receipt["persistence"] = persistence;
        receipt
    };
    if immediate_unload {
        execution_receipt["lifecycle"] =
            unload_after_observation(state, execution, &decision.selected_model).await;
    }
    drop(slot);
    let output_check = super::super::repetition::output_check(&value);
    if path == "/api/chat" {
        // Routing learns from what a model does, not its name: count the run and any loop.
        state.loop_evidence.record(
            &decision.selected_model,
            decision.task,
            output_check
                .as_ref()
                .is_some_and(|check| check["repetition_loop"] == true),
        );
    }
    let mut receipt = json!({
        "route": decision,
        "execution": execution_receipt,
        "admission": {
            "mode": admission_mode,
            "queue_wait_ms": queue_wait_ms,
            "transition_wait_ms": transition_wait_ms,
            "resource_wait_ms": resource_wait_ms,
            "total_wait_ms": queue_wait_ms.saturating_add(transition_wait_ms).saturating_add(resource_wait_ms),
            "slots_total": slots_total,
            "slots_available_during_call": slots_available,
            "cost": cost,
        },
        "metrics": metrics,
        "feedback": feedback_receipt,
        "response": value
    });
    if let Some(check) = output_check {
        receipt["output_check"] = check;
    }
    Ok(Json(receipt))
}
