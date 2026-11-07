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

/// The `/api/ps` entry for `model` on `upstream`, if it is loaded. With several same-name
/// runners, the one carrying `digest` (the catalog identity the caller expects) is preferred.
pub(crate) async fn resident_entry(
    client: &Client,
    upstream: &str,
    model: &str,
    digest: Option<&str>,
) -> Option<Value> {
    let ps = get_json(client, upstream, "/api/ps").await.ok()?;
    crate::ollama_http::find_ps_entry_by_digest(&ps, model, digest).cloned()
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
