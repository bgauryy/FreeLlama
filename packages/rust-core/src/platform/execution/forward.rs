//! Forwarding a managed task to Ollama: retries, completion validation and outcome recording.

use anyhow::Result;
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use std::time::Instant;

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
use crate::proxy;

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

/// Retry 500/502 (load-model blips), but not 503 busy or 504 timeout. Same as passthrough.
/// Retrying 503 while holding an admission slot — and on a cold load, the exclusive write lock —
/// amplifies the saturation the semaphore exists to shed.
fn retryable_managed_status(status: StatusCode) -> bool {
    proxy::retryable_upstream_status(status)
}

/// POST JSON upstream, retrying transient failures on the same backoff schedule the passthrough
/// proxy uses (`proxy::retry_delay`).
///
/// The managed-task path was the one retry-capable caller that had no retries: an Ollama 500 —
/// which it returns under load-model contention, the exact condition managed routing creates —
/// failed the whole task, while the byte-identical request through the passthrough proxy would
/// have survived it. The asymmetry was worse than it looks, because the caller holds the
/// `managed_execution` admission permit across this call: failing bare also threw away an
/// exclusive slot it had already queued for, so the retry it needed was the expensive one to skip.
pub(crate) async fn post_json_with_retries(
    state: &PlatformState,
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let url = crate::ollama_http::url(upstream, path);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let more_attempts = attempt < proxy::MAX_ATTEMPTS;
        match state.client.post(&url).json(body).send().await {
            Ok(response) if retryable_managed_status(response.status()) && more_attempts => {
                eprintln!(
                    "managed task retry attempt={attempt} status={} path={path}",
                    response.status()
                );
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Ok(response) => {
                let status = response.status();
                let bytes = response.bytes().await.map_err(ApiError::upstream)?;
                // A failing Ollama does not always answer in JSON (a wedged runner can return a
                // plain-text or HTML body). A strict parse would turn a truthful 500 into a
                // misleading decode error, so a non-JSON body is carried through as text.
                let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(
                    |_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_owned() }),
                );
                return Ok((status, value));
            }
            // A timeout is NOT a transient hiccup here. This client's per-attempt budget is
            // `platform_task_timeout()` (900s by default), and the caller holds both an admission
            // slot and — on the non-resident path — the exclusive `managed_execution` write lock
            // across every attempt. Retrying a timeout would therefore hold the whole managed
            // plane for up to 3 x 900s, which is exactly the "one hung request deadlocks every
            // subsequent managed task" failure the client timeout was added to prevent. Connection
            // errors are still retried: those fail fast and cost nothing to re-pay.
            Err(error) if more_attempts && error.is_connect() && !error.is_timeout() => {
                eprintln!("managed task retry attempt={attempt} error={error:#} path={path}");
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Err(error) => return Err(ApiError::upstream(error)),
        }
    }
}

/// The response a stopped stream reports: everything generated so far, `done:true` with the
/// stop reason, the streamed token count, and the sidecar's own wall time. Ollama's timing fields
/// arrive only in its final chunk, so they are absent and the sample trains no throughput.
fn streamed_completion(
    last: &Value,
    message: &Value,
    tokens: u64,
    started: Instant,
    reason: &str,
) -> Value {
    json!({
        "model": last["model"],
        "created_at": last["created_at"],
        "message": message,
        "done": true,
        "done_reason": reason,
        "eval_count": tokens,
        "total_duration": u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
    })
}

/// The accumulated assistant message of a stream.
#[derive(Default)]
struct StreamedMessage {
    content: String,
    thinking: String,
    tool_calls: Vec<Value>,
    tokens: u64,
}

impl StreamedMessage {
    fn absorb(&mut self, chunk: &Value) {
        if let Some(piece) = chunk.pointer("/message/content").and_then(Value::as_str)
            && !piece.is_empty()
        {
            self.content.push_str(piece);
            self.tokens += 1;
        }
        if let Some(piece) = chunk.pointer("/message/thinking").and_then(Value::as_str) {
            self.thinking.push_str(piece);
        }
        if let Some(calls) = chunk
            .pointer("/message/tool_calls")
            .and_then(Value::as_array)
        {
            self.tool_calls.extend(calls.iter().cloned());
        }
    }

    fn message(&self) -> Value {
        let mut message = json!({"role": "assistant", "content": self.content});
        if !self.thinking.is_empty() {
            message["thinking"] = json!(self.thinking);
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = json!(self.tool_calls);
        }
        message
    }
}

/// How often, in streamed tokens, a growing output is checked for a loop.
const LOOP_CHECK_TOKENS: u64 = 16;

/// [`post_json_with_retries`] for a streaming `/api/chat` (`stop_on_repetition`): the same retries
/// before the first byte, then Ollama's NDJSON chunks are accumulated into one non-streaming
/// response. Every [`LOOP_CHECK_TOKENS`] tokens the text is checked for a repetition loop; on one,
/// the response is dropped, which closes the connection and cancels the generation upstream, and
/// everything generated so far is returned with `done_reason: "repetition"`. Ollama's own
/// "token repeat limit" abort keeps the generated text too (`done_reason: "repeat_limit"`).
pub(crate) async fn post_streaming_chat_with_retries(
    state: &PlatformState,
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let started = Instant::now();
    let url = crate::ollama_http::url(upstream, path);
    let mut attempt = 0u32;
    let mut response = loop {
        attempt += 1;
        let more_attempts = attempt < proxy::MAX_ATTEMPTS;
        match state.client.post(&url).json(body).send().await {
            Ok(response) if retryable_managed_status(response.status()) && more_attempts => {
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Ok(response) => break response,
            Err(error) if more_attempts && error.is_connect() && !error.is_timeout() => {
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Err(error) => return Err(ApiError::upstream(error)),
        }
    };
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.map_err(ApiError::upstream)?;
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(
            |_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_owned() }),
        );
        return Ok((status, value));
    }
    let mut pending = Vec::new();
    let mut streamed = StreamedMessage::default();
    let mut last = Value::Null;
    let mut checked_at = 0u64;
    while let Some(bytes) = response.chunk().await.map_err(ApiError::upstream)? {
        pending.extend_from_slice(&bytes);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=end).collect::<Vec<_>>();
            let Ok(chunk) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
                if error
                    .as_str()
                    .is_some_and(|text| text.contains("repeat limit"))
                    && !streamed.content.is_empty()
                {
                    let value = streamed_completion(
                        &last,
                        &streamed.message(),
                        streamed.tokens,
                        started,
                        "repeat_limit",
                    );
                    return Ok((StatusCode::OK, value));
                }
                // Any other upstream error keeps the text generated before it.
                let mut value = chunk.clone();
                value["message"] = streamed.message();
                return Ok((status, value));
            }
            streamed.absorb(&chunk);
            if chunk["done"] == true {
                let mut value = chunk;
                value["message"] = streamed.message();
                return Ok((status, value));
            }
            last = chunk;
            if streamed.tokens >= checked_at + LOOP_CHECK_TOKENS {
                checked_at = streamed.tokens;
                if super::super::repetition::should_stop_streaming(&streamed.content) {
                    // Returning drops `response`: the connection closes and Ollama cancels.
                    let value = streamed_completion(
                        &last,
                        &streamed.message(),
                        streamed.tokens,
                        started,
                        "repetition",
                    );
                    return Ok((StatusCode::OK, value));
                }
            }
        }
    }
    // The stream ended without a final chunk: report it as incomplete, keeping the text.
    Ok((
        status,
        json!({"done": false, "message": streamed.message()}),
    ))
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
