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
use super::observe::observe_physical_placement;
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

fn validate_upstream_completion(
    path: &str,
    status: StatusCode,
    value: Value,
) -> Result<Value, ApiError> {
    let reported_error = value.get("error").is_some_and(|error| !error.is_null());
    let incomplete =
        status.is_success() && !reported_error && path == "/api/chat" && value["done"] != true;
    if !status.is_success() || incomplete || reported_error {
        // A completed HTTP exchange can still contain an incomplete generation or an error.
        // Do not train feedback or bind affinity. The pipeline owns any requested unload after
        // execution leases drop; never replay uncertain generation to manufacture completion.
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
        .with_upstream_response(code, value, None));
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
    /// Remains owned until completion and validation. The pipeline owns any later unload.
    pub(crate) slot: AdmissionPermit,
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
        mut decision,
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
        breaker_probe,
    } = request;
    super::identity::verify_before_forward(
        state,
        execution,
        &execution_receipt,
        &decision.selected_model,
    )
    .await?;
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
    // A stream reports a runner that died mid-generation as an error line under HTTP 200.
    let upstream_failed = match &posted {
        Err(_) => true,
        Ok((status, value)) => {
            matches!(status.as_u16(), 500 | 502 | 504)
                || value.get("error").is_some_and(|error| !error.is_null())
        }
    };
    record_upstream_outcome(state, execution, breaker_probe, upstream_failed);
    if upstream_failed {
        observe_completion(state, execution, true, None, None).await;
    }
    let (status, value) = posted?;
    let value = validate_upstream_completion(path, status, value)?;
    let metrics = runtime_metrics(&value);
    let placement = observe_physical_placement(state, execution, &decision.selected_model).await;
    super::identity::verify_completed(&execution_receipt, &placement)?;
    super::identity::finalize_route_evidence(&mut decision, &mut execution_receipt, &placement);
    let feedback_accepted = placement["status"] == "verified";
    execution_receipt["runner_ready"] = super::runner::completion_receipt(
        &execution_receipt["runner_reuse"],
        &placement,
        value["load_duration"].as_u64(),
    );
    // Hash the effective default only when the completed runner proves that exact context.
    // The original request and runtime_options retain Ollama's automatic-mode omission.
    let mut profile_body = body.clone();
    if body["options"].get("num_ctx").is_none()
        && let Some(tokens) =
            execution_receipt["context_sizing"]["ollama_default_context"]["tokens"].as_u64()
        && tokens > 0
        && placement["context_length"].as_u64() == Some(tokens)
    {
        if !profile_body["options"].is_object() {
            profile_body["options"] = json!({});
        }
        profile_body["options"]["num_ctx"] = json!(tokens);
    }
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
        body: &profile_body,
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
    // Measurements are taken while the slot is held; the disk writes happen after it is
    // released, so a slow database never delays the next queued task. Both writes converge in
    // any order (footprints keep the larger size, feedback merges sample ids), so a later task
    // persisting first loses nothing.
    let footprint_save = state.footprints.lock().await.observe(
        &execution.upstream,
        execution_receipt["model_digest"].as_str(),
        &placement,
    );
    execution_receipt["observation"] = placement;
    let slots_total = execution.admission.total();
    // Report throttling rather than hiding it. A caller that fans out embeddings needs to know it
    // is queueing here — otherwise the only symptom is latency it cannot attribute.
    let slots_available = execution.admission.available();
    let (mut feedback_receipt, feedback_snapshot) = {
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
                observation.record_profile_with_phases(
                    profile,
                    duration,
                    super::completion::phase_sample(decision.task, &value, queue_wait_ms),
                );
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
        // Only an accepted sample changes what is worth writing.
        let snapshot = state
            .store
            .clone()
            .filter(|_| feedback_accepted)
            .map(|store| (store, feedback.clone()));
        (receipt, snapshot)
    };
    drop(slot);
    if let Some(save) = footprint_save {
        save.persist().await;
    }
    feedback_receipt["persistence"] = if let Some((store, snapshot)) = feedback_snapshot {
        // A synchronous database write on a runtime worker stalls every task sharing it.
        let written = tokio::task::spawn_blocking(move || persist_feedback(&store, &snapshot))
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("feedback writer panicked: {error}")));
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
    } else if feedback_accepted {
        json!({"enabled": false, "persisted": false})
    } else {
        json!({"enabled": state.store.is_some(), "persisted": false, "reason": "sample_not_accepted"})
    };
    let output_check = super::super::repetition::output_check(&value);
    let artifact_verified = execution_receipt["observation"]["resident"] == true
        && execution_receipt["model_digest"]
            .as_str()
            .is_some_and(|digest| {
                !digest.is_empty()
                    && execution_receipt["observation"]["digest"].as_str() == Some(digest)
            });
    execution_receipt["model_facts_learning"] = json!({
        "accepted": path == "/api/chat" && artifact_verified,
        "reason": if path != "/api/chat" {"not_chat"} else if artifact_verified {"observed_artifact_verified"} else {"observed_artifact_unknown_or_changed"},
    });
    if path == "/api/chat" && artifact_verified {
        // Routing learns from what a model does, not its name: count the run and any loop, and
        // how much room its answer needed.
        let digest = execution_receipt["model_digest"].as_str();
        let looped = output_check
            .as_ref()
            .is_some_and(|check| check["repetition_loop"] == true);
        state
            .model_facts
            .record_run(&decision.selected_model, digest, decision.task, looped);
        state.model_facts.learn_output(
            &decision.selected_model,
            digest,
            decision.options["num_predict"].as_u64(),
            &value,
            looped,
        );
        // Learn this model's bytes per prompt token from a text-only chat, measured exactly as
        // context sizing measures a prompt, so the next estimate uses its own tokenizer.
        let text_only = body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .all(|message| message.get("images").is_none() && message["content"].is_string())
        });
        if text_only && let Some(prompt_tokens) = value["prompt_eval_count"].as_u64() {
            let message_bytes =
                serde_json::to_string(&body["messages"]).map_or(0, |text| text.len());
            state.model_facts.learn_prompt_ratio(
                &decision.selected_model,
                digest,
                u64::try_from(message_bytes).unwrap_or(u64::MAX),
                prompt_tokens,
            );
        }
    }
    let embedding_identity = (path == "/api/embed")
        .then(|| super::identity::embedding_identity(&body, &value, &execution_receipt));
    let mut receipt = json!({
        "route": decision,
        "execution": execution_receipt,
        "admission": {
            "mode": admission_mode,
            // Why the runner was not shared when its identity could not be established,
            // e.g. `resident_identity_unobserved`; null for a plain match or a cold load.
            "reason": execution_receipt["runner_reuse"]["reason"].clone(),
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
    if let Some(identity) = embedding_identity {
        receipt["embedding_identity"] = identity;
    }
    Ok(Json(receipt))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    use crate::platform::{
        PlatformConfig,
        resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
    };

    const GIB: u64 = 1 << 30;

    /// A resident model whose `/api/ps` entry verifies GPU placement, so feedback is accepted.
    async fn resident_ollama() -> (String, tokio::task::JoinHandle<()>) {
        let router = Router::new()
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({"models":[{"name":"m:latest","digest":"digest-m","size":GIB}]}))
                }),
            )
            .route(
                "/api/ps",
                get(|| async {
                    Json(
                        json!({"models":[{"name":"m:latest","model":"m:latest","digest":"digest-m",
                    "size":GIB,"size_vram":GIB,"context_length":2048}]}),
                    )
                }),
            )
            .route(
                "/api/show",
                post(|| async {
                    Json(json!({"capabilities":["completion"],"model_info":{
                    "general.architecture":"llama","llama.context_length":32768}}))
                }),
            )
            .route(
                "/api/chat",
                post(|| async {
                    Json(
                        json!({"message":{"role":"assistant","content":"ok"},"done":true,
                    "eval_count":4,"eval_duration":40_000_000_u64,"prompt_eval_count":8}),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (upstream, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_admission_slot_is_released_before_learned_state_is_written() {
        let (upstream, server) = resident_ollama().await;
        let store = crate::store::Store::in_memory().unwrap();
        let mut config = PlatformConfig::new("127.0.0.1:0", upstream, None, None)
            .with_store(store.clone())
            .with_max_concurrent_tasks(2)
            .with_max_queue_wait(Duration::from_secs(10));
        config.resource_governor =
            ResourceGovernor::with_sampler(ResourcePolicy::default(), || HostResources {
                source: "forward_contract".into(),
                total_memory_bytes: Some(64 * GIB),
                available_memory_bytes: Some(48 * GIB),
                memory_pressure: Some(MemoryPressure::Normal),
                gpu_memory_total_bytes: Some(32 * GIB),
                gpu_memory_free_bytes: Some(24 * GIB),
                gpu_telemetry_source: Some("injected".into()),
                ..HostResources::default()
            })
            .unwrap();
        let (router, state) = crate::platform::build(&config).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        store.stall_until(released);
        let task = tokio::spawn(
            router.oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"task":"completion","model":"m:latest","objective":"fastest",
                            "prompt":"hi","request_options":{"options":{"num_predict":4}}})
                        .to_string(),
                    ))
                    .unwrap(),
            ),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.gpu_admission.receipt()["released"] != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the slot is returned while the database write is still blocked");
        assert!(
            !task.is_finished(),
            "the response waits for its persistence receipt"
        );
        assert_eq!(state.gpu_admission.receipt()["in_flight"], 0);
        release.send(()).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["feedback"]["accepted"], true, "{body}");
        assert_eq!(body["feedback"]["persistence"]["persisted"], true, "{body}");
        server.abort();
    }
}
