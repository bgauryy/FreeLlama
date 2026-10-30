//! Observed residency and physical placement after a task, and post-observation unloads.

use anyhow::Result;
use axum::http::StatusCode;
use reqwest::Client;
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{Backend, PlatformState, get_json};
use super::ExecutionTarget;
use super::upstream::post_json_with_retries;

/// Ask Ollama to unload `model` from `upstream` now (`keep_alive: 0`).
pub(crate) async fn request_unload(
    state: &PlatformState,
    upstream: &str,
    model: &str,
) -> Result<(StatusCode, Value), ApiError> {
    let body = crate::ollama_http::unload_request(model);
    post_json_with_retries(state, upstream, "/api/generate", &body).await
}

pub(crate) async fn unload_after_observation(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
) -> Value {
    match request_unload(state, &execution.upstream, model).await {
        Ok((status, _)) if status.is_success() => {
            let observation = observe_physical_placement(state, execution, model).await;
            let unloaded = observation["status"] == "not_resident";
            json!({
                "requested": "immediate_unload",
                "status": if unloaded { "verified" } else { "failed" },
                "post_unload_observation": observation,
            })
        }
        Ok((status, body)) => json!({
            "requested": "immediate_unload",
            "status": "failed",
            "upstream_status": status.as_u16(),
            "error": body,
        }),
        Err(error) => json!({
            "requested": "immediate_unload",
            "status": "failed",
            "error": error.body.error,
        }),
    }
}

/// Observe the processor that Ollama actually loaded after a managed request. An assignment to
/// the CPU daemon plus `num_gpu:0` is a request, not proof: Metal/MLX builds may still put every
/// byte in VRAM. Unknown/mixed/mismatched observations are returned to the caller and excluded
/// from adaptive feedback so the scheduler cannot learn from a false device label.
pub(crate) async fn observe_physical_placement(
    state: &PlatformState,
    execution: &ExecutionTarget,
    model: &str,
) -> Value {
    let Ok(ps) = get_json(&state.client, &execution.upstream, "/api/ps").await else {
        return json!({
            "processor": "unknown",
            "status": "unavailable",
            "resident": null,
            "source": "ollama_api_ps_after_execution"
        });
    };
    let Some(models) = ps.get("models").and_then(Value::as_array) else {
        return json!({
            "processor": "unknown",
            "status": "unavailable",
            "resident": null,
            "source": "ollama_api_ps_after_execution"
        });
    };
    let running = models
        .iter()
        .find(|entry| crate::ollama_http::ps_entry_name(entry) == Some(model));
    let Some(running) = running else {
        return json!({
            "processor": "unknown",
            "status": "not_resident",
            "resident": false,
            "source": "ollama_api_ps_after_execution"
        });
    };
    let mut observation = physical_placement_observation(
        execution.placement,
        running.get("size").and_then(Value::as_u64),
        running.get("size_vram").and_then(Value::as_u64),
    );
    observation["resident"] = json!(true);
    observation["context_length"] = running
        .get("context_length")
        .cloned()
        .unwrap_or(Value::Null);
    observation["digest"] = running.get("digest").cloned().unwrap_or(Value::Null);
    observation
}

/// The `/api/ps` entry for `model` on `upstream`, if it is loaded.
pub(crate) async fn resident_entry(client: &Client, upstream: &str, model: &str) -> Option<Value> {
    let ps = get_json(client, upstream, "/api/ps").await.ok()?;
    crate::ollama_http::find_ps_entry(&ps, model).cloned()
}

pub(crate) async fn model_is_resident(client: &Client, upstream: &str, model: &str) -> bool {
    resident_entry(client, upstream, model).await.is_some()
}

pub(crate) fn physical_placement_observation(
    requested: Backend,
    size: Option<u64>,
    size_vram: Option<u64>,
) -> Value {
    let processor = match (size, size_vram) {
        (_, Some(0)) => "cpu",
        (Some(size), Some(vram)) if size > 0 && vram >= size => "gpu",
        (Some(size), Some(vram)) if size > 0 && vram > 0 => "mixed",
        _ => "unknown",
    };
    let status = if processor == "unknown" {
        "unavailable"
    } else if processor == requested.as_str() {
        "verified"
    } else {
        "mismatch"
    };
    json!({
        "processor": processor,
        "status": status,
        "source": "ollama_api_ps_after_execution",
        "size": size,
        "size_vram": size_vram,
    })
}
