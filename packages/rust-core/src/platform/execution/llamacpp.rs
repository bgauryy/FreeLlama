//! Explicit, bounded text execution on an operator-run local llama-server.
//! Policy stays in the core: shared weighted pools, memory reservations, deadlines and receipts.
//! This adapter never starts, downloads, unloads or reconfigures a model.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::super::error::{ApiError, resource_error};
use super::super::{
    Backend, ExecutionPreference, ManagedOperation, PlacementEvidence, PlatformState, RouteInput,
    TaskInput, TaskKind, require_active_session, resources::ResourceDemand,
};
use crate::proxy::{LlamaCppConfig, OpenAiUpstream};

pub(in crate::platform) fn target<'a>(
    state: &'a PlatformState,
    model: Option<&str>,
) -> Option<&'a OpenAiUpstream> {
    let (name, _) = model?.split_once('/')?;
    state
        .managed_upstreams
        .iter()
        .find(|upstream| upstream.name == name)
}

fn unsupported(message: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(format!("managed llama.cpp: {message}"))
        .with_code("engine_capability_unsupported")
}

fn validate_route<'a>(
    upstream: &'a OpenAiUpstream,
    route: &RouteInput,
) -> Result<&'a LlamaCppConfig, ApiError> {
    let config = upstream.llamacpp.as_ref().ok_or_else(|| unsupported(
        "this upstream is passthrough-only; configure [upstream.llamacpp] to enable managed tasks",
    ))?;
    if route.model.as_deref() != Some(format!("{}/{}", upstream.name, config.model).as_str()) {
        return Err(unsupported(
            "model must exactly match the operator's configured model",
        ));
    }
    if !matches!(
        route.task,
        TaskKind::Completion | TaskKind::Coding | TaskKind::LongContext
    ) || route
        .required_capabilities
        .iter()
        .any(|capability| *capability != crate::capability::Capability::Completion)
    {
        return Err(unsupported(
            "only text completion, coding and long_context are supported",
        ));
    }
    if route.min_placement_evidence == PlacementEvidence::Observed {
        return Err(unsupported(
            "physical placement observation is unavailable; configured placement is the only evidence",
        ));
    }
    if route.min_confidence.is_some() {
        return Err(unsupported(
            "benchmark confidence is unavailable for this engine",
        ));
    }
    if route
        .context_tokens
        .is_some_and(|tokens| tokens != config.context_tokens)
    {
        return Err(unsupported(
            "context_tokens must equal the operator's fixed server context; per-request resizing is unsupported",
        ));
    }
    if matches!(
        (route.execution_preference, config.processor),
        (ExecutionPreference::PreferCpu, Backend::Gpu)
            | (ExecutionPreference::PreferGpu, Backend::Cpu)
    ) {
        return Err(unsupported(
            "execution preference conflicts with the operator's processor assignment",
        ));
    }
    Ok(config)
}

fn execution_receipt(upstream: &OpenAiUpstream, config: &LlamaCppConfig) -> Value {
    json!({
        "engine": "llamacpp", "backend": "llamacpp", "upstream": upstream.base_url,
        "reason": "explicit_operator_assignment_physical_placement_unknown",
        "requested_processor": config.processor, "placement_evidence": "configured",
        "observed_processor": null,
        "observation": {"processor": "unknown", "status": "unavailable", "source": "not_observed"},
        "physical_placement": {"status": "unknown", "source": "unavailable"},
        "memory_forecast": {"source": "operator_configured", "required_available_bytes": config.memory_bytes,
            "residency_credit": false},
        "context_sizing": {"source": "operator_configured", "context_tokens": config.context_tokens,
            "input_tokens_verified": false},
        "lifecycle": "operator_owned"
    })
}

fn route_receipt(upstream: &OpenAiUpstream, route: &RouteInput) -> Value {
    json!({"selected_model": route.model, "task": route.task, "objective": route.objective,
        "confidence": "low", "task_evidence": "none", "quality_evidence": "none",
        "reasons": ["Explicit operator model; no benchmark qualification or physical placement evidence"], "selection_source": "explicit_operator_assignment",
        "engine": "llamacpp", "upstream_name": upstream.name})
}

async fn bounded_json(mut response: reqwest::Response, limit: usize) -> Result<Value, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ApiError::upstream)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "llama.cpp response exceeds the buffered response limit",
            )
            .with_code("engine_response_invalid"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(StatusCode::BAD_GATEWAY, "llama.cpp returned malformed JSON")
            .with_code("engine_response_invalid")
    })
}

async fn get(
    upstream: &OpenAiUpstream,
    state: &PlatformState,
    path: &str,
) -> Result<Value, ApiError> {
    let mut request = state
        .client
        .get(format!("{}{path}", upstream.base_url))
        .timeout(super::super::config::platform_control_timeout());
    if let Some(key) = &upstream.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(ApiError::upstream)?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "llama.cpp {path} observation failed: HTTP {}",
                response.status()
            ),
        )
        .with_code("engine_observation_unavailable"));
    }
    bounded_json(response, 64 * 1024).await
}

async fn observe(
    state: &PlatformState,
    upstream: &OpenAiUpstream,
    config: &LlamaCppConfig,
) -> Result<(), ApiError> {
    let health = get(upstream, state, "/health").await?;
    if health["status"] != "ok" {
        return Err(
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "llama.cpp is not ready")
                .with_code("engine_observation_unavailable"),
        );
    }
    let list = get(upstream, state, "/models").await?;
    let entries = list["data"]
        .as_array()
        .ok_or_else(|| unsupported("malformed /v1/models response"))?;
    if entries.len() != 1
        || entries[0]["id"] != config.model
        || entries[0]
            .get("status")
            .is_some_and(|status| status["value"] != "loaded")
    {
        return Err(unsupported(
            "requires a ready single-model server listing the exact configured model",
        ));
    }
    Ok(())
}

pub(in crate::platform) async fn preview(
    state: &PlatformState,
    upstream: &OpenAiUpstream,
    route: &RouteInput,
) -> Result<Value, ApiError> {
    let config = validate_route(upstream, route)?;
    observe(state, upstream, config).await?;
    let mut value = route_receipt(upstream, route);
    value["execution"] = execution_receipt(upstream, config);
    value["execution"]["resource_assessment"] =
        json!(state.resources.snapshot().await.assess_demand(
            ResourceDemand::load(config.memory_bytes).on_cpu(config.processor == Backend::Cpu),
            false
        ));
    value["execution"]["preview_only"] = json!(true);
    Ok(value)
}

fn request_body(input: &TaskInput, config: &LlamaCppConfig) -> Result<Value, ApiError> {
    let options = &input.request_options;
    if !matches!(input.operation, ManagedOperation::Generate)
        || input.images.is_some()
        || input.input.is_some()
        || input.nearest.is_some()
        || input.return_embeddings
        || input.state.is_some()
        || input.questions.is_some()
        || input.tools.is_some()
        || input.keep_alive.is_some()
        || input.stream
        || input.stop_on_repetition == Some(true)
        || input.expected_model_digest.is_some()
        || input.expected_execution_backend.is_some()
        || input.expected_upstream.is_some()
        || options.format.is_some()
        || options.think.is_some()
        || options.logprobs.is_some()
        || options.top_logprobs.is_some()
        || options.dimensions.is_some()
    {
        return Err(unsupported(
            "warm, embeddings, tools, images, streaming, early repetition stop, lifecycle, identity pins and advanced Ollama controls are unavailable",
        ));
    }
    let messages = if input.messages.is_empty() {
        let prompt = input
            .prompt
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| unsupported("provide a nonempty prompt or text messages"))?;
        vec![json!({"role": "user", "content": prompt})]
    } else {
        if input.prompt.is_some() {
            return Err(unsupported("provide prompt or messages, not both"));
        }
        input.messages.clone()
    };
    for message in &messages {
        let valid = message.as_object().is_some_and(|object| {
            object.keys().all(|key| key == "role" || key == "content")
                && matches!(
                    message["role"].as_str(),
                    Some("system" | "user" | "assistant")
                )
                && message["content"].is_string()
        });
        if !valid {
            return Err(unsupported(
                "messages must contain only a system/user/assistant role and string content",
            ));
        }
    }
    let mut body = json!({"model": config.model, "messages": messages, "stream": false,
        "max_tokens": config.max_output_tokens});
    for (name, value) in options.options.iter().flatten() {
        match name.as_str() {
            "num_predict"
                if value
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= config.max_output_tokens) =>
            {
                body["max_tokens"] = value.clone();
            }
            "temperature" if value.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0) => {
                body[name] = value.clone();
            }
            "top_p" if value.as_f64().is_some_and(|n| n > 0.0 && n <= 1.0) => {
                body[name] = value.clone();
            }
            "seed" if value.as_i64().is_some() => body[name] = value.clone(),
            "stop"
                if value.is_string()
                    || value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string)) =>
            {
                body[name] = value.clone();
            }
            _ => {
                return Err(unsupported(format!(
                    "unsupported or out-of-range request_options.options.{name}"
                )));
            }
        }
    }
    Ok(body)
}

fn normalize(response: &Value, model: &str) -> Result<Value, ApiError> {
    let choices = response["choices"]
        .as_array()
        .filter(|choices| choices.len() == 1);
    let choice = choices.and_then(|choices| choices.first()).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "llama.cpp returned an invalid completion",
        )
    })?;
    if choice["message"]["role"] != "assistant"
        || !choice["message"]["content"].is_string()
        || choice["message"].get("tool_calls").is_some()
        || !matches!(choice["finish_reason"].as_str(), Some("stop" | "length"))
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "llama.cpp returned incomplete or unsupported output",
        ));
    }
    Ok(
        json!({"model": model, "message": choice["message"], "done": true,
        "done_reason": choice["finish_reason"], "prompt_eval_count": response["usage"]["prompt_tokens"],
        "eval_count": response["usage"]["completion_tokens"]}),
    )
}

struct Capacity {
    _admission: super::super::admission::AdmissionPermit,
    resources: super::super::resources::ResourcePermit,
    charged: u32,
    queued: u128,
}

async fn acquire_capacity(
    state: &PlatformState,
    upstream: &OpenAiUpstream,
    config: &LlamaCppConfig,
    input: &TaskInput,
) -> Result<Capacity, ApiError> {
    let mut execution = super::primary_target(state);
    execution.placement = config.processor;
    execution.upstream.clone_from(&upstream.base_url);
    if config.processor == Backend::Cpu {
        execution.admission = state.cpu_admission.clone();
    }
    let wait = input
        .max_wait_seconds
        .map_or_else(|| state.tunables().max_queue_wait(), Duration::from_secs)
        .min(state.tunables().max_queue_wait());
    let deadline = tokio::time::Instant::now() + wait;
    let (admission, charged, queued) = super::admit(
        state,
        &execution,
        state.tunables().task_cost(input.route.task),
        input.priority,
        &config.model,
        super::admit::QueueDeadline { at: deadline, wait },
        None,
    )
    .await?;
    let demand = ResourceDemand::load(config.memory_bytes).on_cpu(config.processor == Backend::Cpu);
    // Probe once while admitted. A resource hold immediately yields execution units, retaining
    // a bounded queue ticket so resident work sharing this pool can continue.
    let resources = match state
        .resources
        .wait_for_demand(&upstream.base_url, demand, Duration::ZERO)
        .await
    {
        Ok(resources) => resources,
        Err(error) => {
            if tokio::time::Instant::now() >= deadline {
                return Err(resource_error(error));
            }
            let waiter = execution
                .admission
                .register_resource_waiter()
                .map_err(|failure| {
                    super::admit::admission_error(
                        state,
                        &execution,
                        input.priority,
                        charged,
                        failure,
                        wait,
                    )
                })?;
            drop(admission);
            let mut capacity = reserve_and_readmit(
                state,
                &execution,
                input,
                config,
                super::admit::QueueDeadline { at: deadline, wait },
                waiter,
            )
            .await?;
            capacity.queued = capacity.queued.saturating_add(queued);
            return Ok(capacity);
        }
    };
    Ok(Capacity {
        _admission: admission,
        resources,
        charged,
        queued,
    })
}

async fn reserve_and_readmit(
    state: &PlatformState,
    execution: &super::ExecutionTarget,
    input: &TaskInput,
    config: &LlamaCppConfig,
    deadline: super::admit::QueueDeadline,
    waiter: super::super::admission::ResourceWaitGuard,
) -> Result<Capacity, ApiError> {
    if let Some(progress) = &input.job_progress {
        progress.update(
            super::super::jobs::JobStatus::WaitingForResources,
            "waiting_for_safe_memory_and_pressure",
            None,
        );
    }
    let resources = match state
        .resources
        .wait_for_demand(
            &execution.upstream,
            ResourceDemand::load(config.memory_bytes).on_cpu(config.processor == Backend::Cpu),
            deadline
                .at
                .saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
    {
        Ok(resources) => resources,
        Err(error) => {
            waiter.finish();
            return Err(resource_error(error));
        }
    };
    let (admission, charged, requeued) = super::admit(
        state,
        execution,
        state.tunables().task_cost(input.route.task),
        input.priority,
        &config.model,
        deadline,
        Some(waiter),
    )
    .await?;
    // Capacity can change during readmission. Check current pressure without reserving
    // the already-held forecast twice; refuse immediately if it no longer permits work.
    state
        .resources
        .wait_for_demand(
            &execution.upstream,
            ResourceDemand::load(0).on_cpu(config.processor == Backend::Cpu),
            Duration::ZERO,
        )
        .await
        .map_err(resource_error)?;
    Ok(Capacity {
        _admission: admission,
        resources,
        charged,
        queued: requeued,
    })
}

pub(in crate::platform) async fn execute(
    state: &PlatformState,
    upstream: &OpenAiUpstream,
    input: &TaskInput,
) -> Result<Value, ApiError> {
    let config = validate_route(upstream, &input.route)?;
    let body = request_body(input, config)?;
    require_active_session(state, input.route.session_id.as_deref()).await?;
    if let Some(observer) = &input.route_observer {
        observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&input.route.model);
    }
    observe(state, upstream, config).await?;
    if let Some(progress) = &input.job_progress {
        progress.route(input.route.model.as_deref().unwrap_or_default(), "llamacpp");
        progress.update(
            super::super::jobs::JobStatus::Queued,
            "waiting_for_capacity",
            None,
        );
    }
    let capacity = acquire_capacity(state, upstream, config, input).await?;
    require_active_session(state, input.route.session_id.as_deref()).await?;
    let mut request = state
        .client
        .post(format!("{}/chat/completions", upstream.base_url))
        .json(&body);
    if let Some(key) = &upstream.api_key {
        request = request.bearer_auth(key);
    }
    if let Some(progress) = &input.job_progress {
        progress.update(super::super::jobs::JobStatus::Running, "generating", None);
    }
    // Never replay an accepted or uncertain POST, including HTTP errors and timeouts.
    let response = request.send().await.map_err(ApiError::upstream)?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "llama.cpp generation returned HTTP {}; execution outcome may be partial",
                response.status()
            ),
        )
        .with_code("engine_execution_failed"));
    }
    let raw = bounded_json(response, state.max_request_bytes).await?;
    let normalized = normalize(&raw, input.route.model.as_deref().unwrap_or_default())?;
    let mut receipt = json!({"route": route_receipt(upstream, &input.route),
        "execution": execution_receipt(upstream, config), "response": normalized,
        "metrics": {"prompt_tokens": raw["usage"]["prompt_tokens"], "output_tokens": raw["usage"]["completion_tokens"]},
        "admission": {"cost": capacity.charged, "total_wait_ms": capacity.queued}, "resources": capacity.resources.receipt});
    receipt["execution"]["request_max_output_tokens"] = body["max_tokens"].clone();
    if let Some(check) = super::super::repetition::output_check(&receipt["response"]) {
        receipt["output_check"] = check;
    }
    Ok(receipt)
}
