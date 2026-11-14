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
    digest: Option<&str>,
    context: Option<u64>,
) -> Value {
    let Ok(ps) = get_json(&state.client, &execution.upstream, "/api/ps").await else {
        return json!({
            "processor": "unknown",
            "status": "unavailable",
            "resident": null,
            "source": "ollama_api_ps_after_execution"
        });
    };
    placement_from_listing(&ps, execution.placement, model, digest, context)
}

/// A tag can have several resident revisions or contexts. Device counters from the first
/// same-name runner are not evidence about this task; require one unambiguous identity match.
fn placement_from_listing(
    ps: &Value,
    requested: Backend,
    model: &str,
    digest: Option<&str>,
    context: Option<u64>,
) -> Value {
    let Some(models) = ps.get("models").and_then(Value::as_array) else {
        return json!({
            "processor": "unknown",
            "status": "unavailable",
            "resident": null,
            "source": "ollama_api_ps_after_execution"
        });
    };
    let present = models
        .iter()
        .any(|entry| crate::ollama_http::ps_entry_name(entry) == Some(model));
    if !present {
        return json!({
            "processor": "unknown",
            "status": "not_resident",
            "resident": false,
            "source": "ollama_api_ps_after_execution"
        });
    }
    let unknown = |reason| {
        json!({
            "processor":"unknown", "status":"unavailable", "resident":true,
            "source":"ollama_api_ps_after_execution", "reason":reason,
            "expected_digest":digest, "expected_context_length":context,
        })
    };
    let Some(digest) = digest.filter(|digest| !digest.is_empty()) else {
        return unknown("expected_runner_digest_unavailable");
    };
    let Some(context) = context.filter(|tokens| *tokens > 0) else {
        return unknown("expected_runner_context_unavailable");
    };
    let mut matching = models.iter().filter(|entry| {
        crate::ollama_http::ps_entry_name(entry) == Some(model)
            && entry["digest"].as_str() == Some(digest)
            && entry["context_length"].as_u64() == Some(context)
    });
    let Some(running) = matching.next() else {
        return unknown("matching_runner_identity_unavailable");
    };
    if matching.next().is_some() {
        return unknown("matching_runner_identity_ambiguous");
    }
    let mut observation = physical_placement_observation(
        requested,
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

#[cfg(test)]
mod tests {
    use super::placement_from_listing;
    use crate::platform::Backend;
    use serde_json::{Value, json};

    fn observe(entries: &Value, digest: Option<&str>, context: Option<u64>) -> Value {
        placement_from_listing(
            &json!({"models":entries}),
            Backend::Gpu,
            "model:tag",
            digest,
            context,
        )
    }

    fn runner(digest: &str, context: u64, vram: u64) -> Value {
        json!({"name":"model:tag","digest":digest,"context_length":context,"size":100,"size_vram":vram})
    }

    #[test]
    fn post_run_placement_selects_the_requested_revision_and_context() {
        let correct = runner("current", 4096, 0);
        let result = observe(
            &json!([
                runner("old", 4096, 100),
                runner("current", 2048, 100),
                correct
            ]),
            Some("current"),
            Some(4096),
        );
        assert_eq!(result["digest"], "current");
        assert_eq!(result["context_length"], 4096);
        assert_eq!(result["processor"], "cpu");
        assert_eq!(result["status"], "mismatch");
    }

    #[test]
    fn missing_or_ambiguous_runner_identity_never_verifies_placement() {
        let correct = runner("current", 4096, 100);
        for (entries, digest, context) in [
            (json!([correct.clone()]), None, Some(4096)),
            (json!([correct.clone()]), Some("current"), None),
            (
                json!([runner("old", 4096, 100)]),
                Some("current"),
                Some(4096),
            ),
            (
                json!([runner("current", 2048, 100)]),
                Some("current"),
                Some(4096),
            ),
            (
                json!([correct.clone(), correct]),
                Some("current"),
                Some(4096),
            ),
        ] {
            let result = observe(&entries, digest, context);
            assert_eq!(result["processor"], "unknown", "{result}");
            assert_eq!(result["status"], "unavailable", "{result}");
            assert_eq!(result["resident"], true, "{result}");
        }
        let missing = observe(&json!([]), Some("current"), Some(4096));
        assert_eq!(missing["status"], "not_resident");
        assert_eq!(missing["resident"], false);
    }
}
