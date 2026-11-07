//! Building the Ollama request body for a managed task, and its admission cost.

use crate::capability::Capability;
use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{
    Backend, CatalogModel, OllamaRequestOptions, RouteDecision, TaskInput, TaskKind,
};
use super::ExecutionTarget;

pub(crate) fn requests_immediate_unload(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("0" | "0s" | "0m" | "0h"))
}

/// An explicit operator override for CPU-placed runners. Otherwise Ollama detects its own
/// thread count. Guessing physical cores from half the logical CPUs is inaccurate on many
/// topologies and creates avoidable runner-option mismatches with other clients. Explicit
/// thread settings remain unverified by /api/ps and use conservative exclusive admission.
fn cpu_num_thread() -> Option<u64> {
    std::env::var("FREELLAMA_CPU_NUM_THREAD")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
}

pub(crate) fn apply_execution_options(body: &mut Value, target: &ExecutionTarget) {
    if target.placement == Backend::Cpu {
        if let (Some(options), Some(threads)) = (
            body.get_mut("options").and_then(Value::as_object_mut),
            cpu_num_thread(),
        ) {
            options
                .entry("num_thread")
                .or_insert_with(|| json!(threads));
        }
        // The process-level CPU library override is ignored by some Metal builds. Ollama's
        // request contract treats num_gpu as a runner load option; pinning zero here makes the
        // explicit CPU assignment real while the second process prevents GPU-runner churn on the
        // primary backend.
        body["options"]["num_gpu"] = json!(0);
    }
}

pub(crate) fn apply_request_options(
    task: TaskKind,
    decision: &mut RouteDecision,
    request: &OllamaRequestOptions,
) -> Result<(), ApiError> {
    for reserved in ["num_ctx", "num_gpu"] {
        if request
            .options
            .as_ref()
            .is_some_and(|options| options.contains_key(reserved))
        {
            return Err(ApiError::bad_request(anyhow::anyhow!(
                "request_options.options.{reserved} is routing-owned; use context_tokens for num_ctx and execution_preference/operator backend assignment for num_gpu"
            )));
        }
    }
    if let Some(format) = &request.format
        && !matches!(format, Value::Object(_))
        && format.as_str() != Some("json")
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.format must be \"json\" or a JSON schema object"
        )));
    }
    if let Some(think) = &request.think
        && !think.is_null()
        && !think.is_boolean()
        && think.as_str().is_none_or(|level| level.trim().is_empty())
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.think must be a boolean, null, or a non-empty model-defined thinking level"
        )));
    }
    if request.top_logprobs.is_some() && request.logprobs != Some(true) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.top_logprobs requires logprobs=true"
        )));
    }
    if matches!(task, TaskKind::Decision)
        && (request.options.is_some()
            || request.format.is_some()
            || request.think.is_some()
            || request.logprobs.is_some()
            || request.top_logprobs.is_some())
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "decision tasks take state and questions only; /v1/systemone accepts no options, format, think, or logprobs"
        )));
    }
    if matches!(task, TaskKind::Embedding)
        && (request.format.is_some()
            || request.think.is_some()
            || request.logprobs.is_some()
            || request.top_logprobs.is_some())
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "embedding tasks accept request_options.options only; format, think, and logprobs are chat controls"
        )));
    }
    let options = decision
        .options
        .as_object_mut()
        .expect("route options are always an object");
    options.extend(request.options.clone().unwrap_or_default());
    if let Some(think) = &request.think {
        decision.think = think.clone();
    }
    Ok(())
}

/// Explicit controls must match known model metadata. Unknown metadata leaves Ollama as the
/// authority; unlike a global enum, this admits future levels advertised by the selected model.
pub(crate) fn validate_model_controls(
    model: &CatalogModel,
    request: &OllamaRequestOptions,
) -> Result<(), ApiError> {
    let Some(think) = request.think.as_ref().filter(|think| !think.is_null()) else {
        return Ok(());
    };
    let Some(values) = model
        .thinking
        .as_ref()
        .and_then(|thinking| thinking["values"].as_array())
    else {
        return Ok(());
    };
    let supported = match think {
        Value::Bool(true) => values
            .iter()
            .any(|value| value == true || value.is_string()),
        Value::Bool(false) => values.iter().any(|value| value == false),
        Value::String(_) => values.contains(think),
        _ => false,
    };
    if supported {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "model {} does not advertise think={think}; use one of its /api/show thinking.values: {}",
        model.name,
        Value::Array(values.clone())
    ))
    .with_code("unsupported_thinking_control"))
}

/// Derive requirements from the actual payload before routing. Ollama's legacy `images` field
/// also carries WAV audio; its name alone is not evidence that the input requires vision.
pub(crate) fn derive_input_capabilities(input: &mut TaskInput) -> Result<(), ApiError> {
    if input.tools.is_some() {
        input.route.required_capabilities.insert(Capability::Tools);
    }
    if input
        .request_options
        .think
        .as_ref()
        .is_some_and(|think| think == true || think.is_string())
    {
        input
            .route
            .required_capabilities
            .insert(Capability::Thinking);
    }
    let mut media = Vec::new();
    if let Some(images) = &input.images {
        media.extend(
            images
                .iter()
                .map(|data| media_capability(data, Capability::Vision)),
        );
    }
    for message in &input.messages {
        if let Some(images) = message["images"].as_array() {
            media.extend(
                images
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|data| media_capability(data, Capability::Vision)),
            );
        }
    }
    if matches!(input.route.task, TaskKind::Embedding)
        && let Some(value) = &input.input
    {
        let items = value
            .as_array()
            .map_or_else(|| vec![value], |items| items.iter().collect());
        for item in items {
            match item {
                Value::String(_) => {}
                Value::Object(fields) => {
                    if fields.is_empty() || fields.values().all(|value| value.as_str() == Some(""))
                    {
                        return Err(ApiError::bad_request(
                            "embedding input item has no text, image, or audio",
                        ));
                    }
                    for (key, value) in fields {
                        let Some(text) = value.as_str() else {
                            return Err(ApiError::bad_request(format!(
                                "embedding input.{key} must be a string"
                            )));
                        };
                        match key.as_str() {
                            "text" => {}
                            "image" | "audio" if !text.is_empty() => media.push(media_capability(
                                text,
                                if key == "audio" {
                                    Capability::Audio
                                } else {
                                    Capability::Vision
                                },
                            )),
                            "image" | "audio" => {
                                return Err(ApiError::bad_request(format!(
                                    "embedding input.{key} must not be empty"
                                )));
                            }
                            _ => {
                                return Err(ApiError::bad_request(format!(
                                    "unsupported embedding input field {key}; use text, image, or audio"
                                )));
                            }
                        }
                    }
                }
                _ => {
                    return Err(ApiError::bad_request(
                        "embedding input must be text, a text/image/audio object, or an array of those items",
                    ));
                }
            }
        }
    }
    input.route.required_capabilities.extend(media);
    Ok(())
}

fn media_capability(data: &str, declared: Capability) -> Capability {
    // Decode only the RIFF/WAVE header; payload validation and decoding belong to Ollama.
    let mut header = [0u8; 12];
    let prefix = data.get(..16).unwrap_or(data);
    if base64::engine::general_purpose::STANDARD
        .decode_slice(prefix, &mut header)
        .ok()
        == Some(12)
        && &header[..4] == b"RIFF"
        && &header[8..12] == b"WAVE"
    {
        Capability::Audio
    } else {
        declared
    }
}

/// A generation needs something to work on: an embedding needs `input`; a chat needs messages,
/// or a prompt with text or images. Checked before routing, so an empty request never costs a
/// model load.
pub(crate) fn require_payload(input: &TaskInput) -> Result<(), ApiError> {
    if matches!(input.operation, super::super::ManagedOperation::Warm) {
        return Ok(());
    }
    if matches!(input.route.task, TaskKind::Decision) {
        return require_decision_payload(input);
    }
    if matches!(input.route.task, TaskKind::Embedding) {
        if input.prompt.is_some()
            || !input.messages.is_empty()
            || input.images.is_some()
            || input.tools.is_some()
            || input.state.is_some()
            || input.questions.is_some()
        {
            return Err(ApiError::bad_request(
                "embedding tasks take input; chat and decision payload fields are not accepted",
            ));
        }
        let empty = match &input.input {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.is_empty(),
            Some(Value::Array(items)) => items.is_empty(),
            Some(_) => false,
        };
        return if empty {
            Err(ApiError::bad_request(anyhow::anyhow!(
                "embedding task requires a non-empty input; use warm to load the model"
            )))
        } else {
            Ok(())
        };
    }
    let has_text = input
        .prompt
        .as_deref()
        .is_some_and(|prompt| !prompt.trim().is_empty());
    let has_images = input
        .images
        .as_ref()
        .is_some_and(|images| !images.is_empty());
    if input.messages.is_empty() && !has_text && !has_images {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "task requires messages, or a prompt with text or images; use warm to load a model without generating"
        )));
    }
    Ok(())
}

/// A decision needs a state and at least one named question; chat fields have no meaning there,
/// so sending them is refused rather than silently dropped.
fn require_decision_payload(input: &TaskInput) -> Result<(), ApiError> {
    let state_present = match &input.state {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(_) => true,
    };
    if !state_present {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "decision task requires a non-empty state: the text or JSON the questions are about"
        )));
    }
    if input
        .questions
        .as_ref()
        .and_then(Value::as_object)
        .is_none_or(serde_json::Map::is_empty)
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "decision task requires questions: an object of named questions, each {{\"type\":\"noul\"|\"choice\"|\"score\",\"instructions\":...,\"criteria\":...}}"
        )));
    }
    if !input.messages.is_empty()
        || input.prompt.is_some()
        || input.input.is_some()
        || input.tools.is_some()
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "decision tasks take state and questions; prompt, messages, input, and tools belong to other tasks"
        )));
    }
    Ok(())
}

/// Ollama embeds each item whole (`truncate: false`) and fails the batch, after loading the
/// model, when one item is longer than the model's context. Refuse before admission the items
/// that certainly cannot fit: more than 8 bytes per token of the window, a ratio real text stays
/// well under, so nothing that might fit is refused. The caller splits them; `FreeLlama` does not,
/// because where to cut changes what an embedding means.
fn refuse_oversized_embedding_items(input: &Value, window: Option<u64>) -> Result<(), ApiError> {
    let Some(window) = window.filter(|window| *window > 0) else {
        return Ok(());
    };
    let limit = window.saturating_mul(8);
    let items: Vec<&str> = match input {
        Value::String(text) => vec![text.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let oversized: Vec<Value> = items
        .iter()
        .enumerate()
        .filter(|(_, text)| text.len() as u64 > limit)
        .map(|(index, text)| json!({"index": index, "bytes": text.len()}))
        .collect();
    if oversized.is_empty() {
        return Ok(());
    }
    Err(ApiError::new(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        format!(
            "{} embedding item(s) cannot fit this model's {window}-token context (more than {limit} bytes each): {}; split each into chunks of about {} bytes or less and embed the chunks",
            oversized.len(),
            Value::Array(oversized.clone()),
            window.saturating_mul(3),
        ),
    )
    .with_code("embedding_item_exceeds_context"))
}

pub(crate) fn build_managed_request(
    input: &mut TaskInput,
    decision: &RouteDecision,
    keep_alive: &Value,
) -> Result<(&'static str, Value), ApiError> {
    if matches!(input.route.task, TaskKind::Decision) {
        return build_decision_request(input, decision, keep_alive);
    }
    if matches!(input.operation, super::super::ManagedOperation::Warm) {
        // Ollama loads a model on an empty request: no messages for chat, no input for embed.
        if matches!(input.route.task, TaskKind::Embedding) {
            if !input.messages.is_empty() {
                return Err(ApiError::bad_request(
                    "warm messages fill a chat model's prompt cache; embedding warms take none",
                ));
            }
            return Ok((
                "/api/embed",
                json!({
                    "model": decision.selected_model,
                    "input": [],
                    "truncate": false,
                    "keep_alive": keep_alive,
                    "options": decision.options,
                }),
            ));
        }
        // With messages, the runner evaluates them into its prompt cache and generates nothing
        // (`num_predict: 0`), so the next request that starts with them reuses that work.
        let messages = std::mem::take(&mut input.messages);
        let mut options = decision.options.clone();
        if !messages.is_empty() {
            options["num_predict"] = json!(0);
        }
        return Ok((
            "/api/chat",
            json!({
                "model": decision.selected_model,
                "messages": messages,
                "stream": false,
                "truncate": false,
                "shift": false,
                "keep_alive": keep_alive,
                "options": options,
            }),
        ));
    }
    if matches!(input.route.task, TaskKind::Embedding) {
        let value = input
            .input
            .take()
            .context("embedding task requires input")
            .map_err(ApiError::bad_request)?;
        refuse_oversized_embedding_items(&value, decision.options["num_ctx"].as_u64())?;
        return Ok((
            "/api/embed",
            json!({
                "model": decision.selected_model,
                "input": value,
                "truncate": false,
                "keep_alive": keep_alive,
                "options": decision.options,
            }),
        ));
    }

    let messages = if input.messages.is_empty() {
        let mut message = json!({
            "role": "user",
            "content": input.prompt.take().context("task requires prompt or messages").map_err(ApiError::bad_request)?
        });
        if let Some(images) = input.images.take() {
            message["images"] = json!(images);
        }
        vec![message]
    } else {
        std::mem::take(&mut input.messages)
    };
    let mut body = json!({
        "model": decision.selected_model,
        "messages": messages,
        // Streaming lets the forward path stop a repetition loop early; see `stop_on_repetition`.
        // On by default only for free text with nothing capping it: JSON and tool calls can be
        // legitimately periodic, and a cut there leaves an invalid answer.
        "stream": input.stop_on_repetition.unwrap_or_else(|| {
            decision.options.get("num_predict").is_none()
                && input.tools.is_none()
                && input.request_options.format.is_none()
        }),
        "truncate": false,
        "shift": false,
        "keep_alive": keep_alive,
        "options": decision.options,
    });
    body.as_object_mut()
        .expect("request body is an object")
        .extend(
            profile_controls(input, decision)
                .as_object()
                .expect("controls are an object")
                .clone(),
        );
    Ok(("/api/chat", body))
}

/// The exact controls shared by request forwarding and profile prediction. Keeping payloads
/// out of this projection avoids cloning large prompts, histories or images for each candidate.
pub(super) fn profile_controls(input: &TaskInput, decision: &RouteDecision) -> Value {
    let mut controls = json!({"options":decision.options});
    if matches!(input.route.task, TaskKind::Embedding) {
        return controls;
    }
    if !decision.think.is_null() {
        controls["think"] = decision.think.clone();
    }
    if let Some(tools) = &input.tools {
        controls["tools"] = tools.clone();
    }
    if let Some(format) = &input.request_options.format {
        controls["format"] = format.clone();
    }
    if let Some(logprobs) = input.request_options.logprobs {
        controls["logprobs"] = json!(logprobs);
    }
    if let Some(top_logprobs) = input.request_options.top_logprobs {
        controls["top_logprobs"] = json!(top_logprobs);
    }
    controls
}

/// `/v1/systemone` takes no options: Ollama sizes the runner, so only what it reads is sent.
fn build_decision_request(
    input: &mut TaskInput,
    decision: &RouteDecision,
    keep_alive: &Value,
) -> Result<(&'static str, Value), ApiError> {
    if matches!(input.operation, super::super::ManagedOperation::Warm) {
        return Err(ApiError::bad_request(
            "decision models load on their first question; /v1/systemone has no empty request to warm with",
        ));
    }
    let mut body = json!({
        "model": decision.selected_model,
        "state": input.state.take(),
        "questions": input.questions.take(),
        "keep_alive": keep_alive,
    });
    if let Some(images) = input.images.take().filter(|images| !images.is_empty()) {
        body["images"] = json!(images);
    }
    Ok(("/v1/systemone", body))
}

pub(crate) fn task_has_images(input: &TaskInput) -> bool {
    input
        .images
        .iter()
        .flatten()
        .any(|data| media_capability(data, Capability::Vision) == Capability::Vision)
        || input
            .messages
            .iter()
            .flat_map(|message| message["images"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .any(|data| media_capability(data, Capability::Vision) == Capability::Vision)
}

/// Charge an embedding batch for its actual cardinality. Ollama executes a batched `/api/embed`
/// request as more work than a single string even though it remains materially cheaper than N
/// independent HTTP calls. Arithmetic saturates at the maximum supported weight; `admit` applies
/// the backend-specific cap when the task acquires its permit.
pub(crate) fn task_cost_for(
    task: TaskKind,
    batch_items: usize,
    tunables: &super::super::runtime::Tunables,
) -> u32 {
    let base = tunables.task_cost(task);
    if !matches!(task, TaskKind::Embedding) {
        return base;
    }
    let items = u32::try_from(batch_items.max(1)).unwrap_or(u32::MAX);
    // The configured base covers each group of up to four inputs. This is a queueing weight,
    // not a VRAM estimate.
    items.div_ceil(4).saturating_mul(base)
}

pub(crate) fn input_batch_items(input: &TaskInput) -> usize {
    input
        .input
        .as_ref()
        .and_then(Value::as_array)
        .map_or(1, Vec::len)
        .max(1)
}

#[cfg(test)]
#[path = "request_contract_tests.rs"]
mod contract_tests;
