//! Building the Ollama request body for a managed task, and its admission cost.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{Backend, OllamaRequestOptions, RouteDecision, TaskInput, TaskKind};
use super::ExecutionTarget;

pub(crate) fn requests_immediate_unload(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("0" | "0s" | "0m" | "0h"))
}

/// Thread count for CPU-placed runners, or `None` to let Ollama choose.
///
/// Ollama compares `num_thread` when deciding whether a loaded runner can serve a request, so a
/// value that differs from what other clients send forces a reload. It is therefore set only on
/// the dedicated CPU backend, where `FreeLlama` is the only client. The GPU backend is left alone:
/// a forced value there makes every raw request without `num_thread` reload the runner managed
/// traffic loaded, and on Apple Silicon (no SMT) `logical/2` halves CPU throughput.
fn cpu_num_thread() -> Option<u64> {
    if let Some(value) = std::env::var("FREELLAMA_CPU_NUM_THREAD")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
    {
        return Some(value);
    }
    if cfg!(target_os = "macos") {
        // Ollama already sizes to performance cores; efficiency cores slow llama.cpp's barriers.
        return None;
    }
    // x86 SMT: half the logical CPUs approximates physical cores and leaves the siblings free.
    std::thread::available_parallelism()
        .ok()
        .map(|cores| (cores.get() as u64 / 2).max(1))
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
        && !think.is_boolean()
        && !matches!(think.as_str(), Some("low" | "medium" | "high"))
    {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.think must be a boolean or one of low, medium, high"
        )));
    }
    if request.top_logprobs.is_some() && request.logprobs != Some(true) {
        return Err(ApiError::bad_request(anyhow::anyhow!(
            "request_options.top_logprobs requires logprobs=true"
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

pub(crate) fn build_managed_request(
    input: &mut TaskInput,
    decision: &RouteDecision,
    keep_alive: &Value,
) -> Result<(&'static str, Value), ApiError> {
    if matches!(
        input.operation,
        super::super::warming::ManagedOperation::Warm
    ) {
        return Ok((
            "/api/chat",
            json!({
                "model": decision.selected_model,
                "messages": [],
                "stream": false,
                "truncate": false,
                "shift": false,
                "keep_alive": keep_alive,
                "options": decision.options,
            }),
        ));
    }
    if matches!(input.route.task, TaskKind::Embedding) {
        let value = input
            .input
            .take()
            .context("embedding task requires input")
            .map_err(ApiError::bad_request)?;
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
        "stream": input.stop_on_repetition,
        "truncate": false,
        "shift": false,
        "keep_alive": keep_alive,
        "options": decision.options,
    });
    if !decision.think.is_null() {
        body["think"] = decision.think.clone();
    }
    if let Some(tools) = input.tools.take() {
        body["tools"] = tools;
    }
    if let Some(format) = input.request_options.format.take() {
        body["format"] = format;
    }
    if let Some(logprobs) = input.request_options.logprobs {
        body["logprobs"] = json!(logprobs);
    }
    if let Some(top_logprobs) = input.request_options.top_logprobs {
        body["top_logprobs"] = json!(top_logprobs);
    }
    Ok(("/api/chat", body))
}

pub(crate) fn task_has_images(input: &TaskInput) -> bool {
    input
        .images
        .as_ref()
        .is_some_and(|images| !images.is_empty())
        || input.messages.iter().any(|message| {
            message
                .get("images")
                .and_then(Value::as_array)
                .is_some_and(|images| !images.is_empty())
        })
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
