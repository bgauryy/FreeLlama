//! Artifact identity contracts for work that must continue against the same model and backend.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::super::{Backend, PlatformState, RouteDecision, TaskInput, error::ApiError, get_json};
use super::{ExecutionTarget, ManagedDecision};

fn mismatch(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message.into()).with_code("model_identity_changed")
}

fn present(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

pub(super) fn verify_requested(
    input: &TaskInput,
    managed: &ManagedDecision,
    receipt: &mut Value,
) -> Result<(), ApiError> {
    let caller_contract = input.expected_model_digest.is_some()
        || input.expected_execution_backend.is_some()
        || input.expected_upstream.is_some();
    let evidence_contract = input.route.model.is_none()
        && (managed.route.quality_evidence == "strong" || managed.route.task_evidence == "strong");
    if !caller_contract && !evidence_contract {
        return Ok(());
    }
    if caller_contract && input.route.model.is_none() {
        return Err(ApiError::bad_request(
            "expected model identity requires an explicit model tag",
        ));
    }
    for (name, value) in [
        ("expected_model_digest", &input.expected_model_digest),
        (
            "expected_execution_backend",
            &input.expected_execution_backend,
        ),
        ("expected_upstream", &input.expected_upstream),
    ] {
        if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err(ApiError::bad_request(format!("{name} must be non-empty")));
        }
    }
    if input
        .expected_execution_backend
        .as_deref()
        .is_some_and(|value| !matches!(value, "cpu" | "primary"))
    {
        return Err(ApiError::bad_request(
            "expected_execution_backend must be cpu or primary",
        ));
    }
    receipt["expected_identity"] = json!({
        "model": managed.route.selected_model,
        "model_digest": input.expected_model_digest.as_deref().or_else(|| evidence_contract.then_some(managed.model.digest.as_deref()).flatten()),
        "backend": input.expected_execution_backend.as_deref().or_else(|| evidence_contract.then_some(if managed.execution.placement == Backend::Cpu {"cpu"} else {"primary"})),
        "upstream": input.expected_upstream.as_deref().or_else(|| evidence_contract.then_some(managed.execution.upstream.as_str())),
        "source": if caller_contract {"caller_expected_identity"} else {"automatic_evidence_qualified_selection"},
    });
    if evidence_contract && present(&receipt["expected_identity"]["model_digest"]).is_none() {
        return Err(mismatch(
            "quality-qualified selection has no artifact identity; requalification is required",
        ));
    }
    verify_selection(
        receipt,
        &managed.route.selected_model,
        if managed.execution.placement == Backend::Cpu {
            "cpu"
        } else {
            "primary"
        },
        &managed.execution.upstream,
    )
}

/// The selected catalog already carries the discovery identity. Validate that local evidence
/// before admission without another request; dispatch below still obtains fresh tags after waits.
fn verify_selection(
    receipt: &Value,
    model: &str,
    backend: &str,
    upstream: &str,
) -> Result<(), ApiError> {
    let expected = &receipt["expected_identity"];
    for (field, actual) in [
        ("model", model),
        ("backend", backend),
        ("upstream", upstream),
    ] {
        if present(&expected[field]).is_some_and(|wanted| wanted != actual) {
            return Err(mismatch(format!(
                "selected {field} changed; refusing continued work"
            )));
        }
    }
    if let Some(wanted) = present(&expected["model_digest"])
        && present(&receipt["model_digest"]) != Some(wanted)
    {
        return Err(mismatch(
            "selected catalog model digest changed or cannot be verified; refusing continued work",
        ));
    }
    Ok(())
}

fn verify_tags(
    tags: &Value,
    receipt: &Value,
    model: &str,
    backend: &str,
    upstream: &str,
) -> Result<(), ApiError> {
    verify_selection(receipt, model, backend, upstream)?;
    let expected = &receipt["expected_identity"];
    let current = tags["models"]
        .as_array()
        .and_then(|models| {
            models.iter().find(|entry| {
                entry["name"].as_str().or_else(|| entry["model"].as_str()) == Some(model)
            })
        })
        .and_then(|entry| present(&entry["digest"]));
    if let Some(wanted) = present(&expected["model_digest"])
        && current != Some(wanted)
    {
        return Err(mismatch(
            "selected model digest changed or cannot be verified; refusing continued work",
        ));
    }
    Ok(())
}

/// Recheck after queueing, after the initial catalog check: another client can replace a tag while
/// this request waits. Ollama has no atomic digest-bound inference API, so post-run evidence is
/// also required and a changed/unknown observation must never produce a successful continuation.
pub(super) async fn verify_before_forward(
    state: &PlatformState,
    execution: &ExecutionTarget,
    receipt: &Value,
    model: &str,
) -> Result<(), ApiError> {
    if receipt.get("expected_identity").is_none() {
        return Ok(());
    }
    let backend = if execution.placement == Backend::Cpu {
        "cpu"
    } else {
        "primary"
    };
    // Verify configured identity before issuing even a read to a changed endpoint.
    verify_selection(receipt, model, backend, &execution.upstream)?;
    // A backend/endpoint-only contract is fully checked above. It neither requests an artifact
    // assertion nor depends on another metadata request succeeding.
    if present(&receipt["expected_identity"]["model_digest"]).is_none() {
        return Ok(());
    }
    let tags = get_json(&state.client, &execution.upstream, "/api/tags")
        .await
        .map_err(|_| mismatch("current model identity is unavailable; refusing continued work"))?;
    verify_tags(&tags, receipt, model, backend, &execution.upstream)
}

pub(super) fn verify_completed(receipt: &Value, placement: &Value) -> Result<(), ApiError> {
    if let Some(wanted) = present(&receipt["expected_identity"]["model_digest"])
        && (placement["resident"] != true || present(&placement["digest"]) != Some(wanted))
    {
        return Err(mismatch(
            "completed model digest changed or cannot be observed; discard this result and restart continued work",
        ));
    }
    Ok(())
}

/// An explicit operator pin may still execute when it has no strict identity contract. Its
/// receipt must not advertise measured quality for an artifact that execution could not prove.
pub(super) fn finalize_route_evidence(
    decision: &mut RouteDecision,
    receipt: &mut Value,
    placement: &Value,
) {
    if decision.quality_evidence != "strong" && decision.task_evidence != "strong" {
        return;
    }
    let verified = placement["resident"] == true
        && present(&receipt["model_digest"])
            .is_some_and(|digest| present(&placement["digest"]) == Some(digest));
    receipt["qualification_identity"] = json!({
        "status": if verified {"verified"} else {"unverified"},
        "catalog_digest": receipt["model_digest"],
        "observed_digest": placement["digest"],
    });
    if !verified {
        "low".clone_into(&mut decision.confidence);
        "capability_metadata_only".clone_into(&mut decision.evidence);
        "none".clone_into(&mut decision.quality_evidence);
        "none".clone_into(&mut decision.task_evidence);
        decision.reasons.retain(|reason| {
            !matches!(
                reason.as_str(),
                "configured_task_policy" | "functional_screen_rank"
            )
        });
        decision
            .reasons
            .push("measured_evidence_cleared_unverified_artifact".to_owned());
    }
}

/// Observed embedding runner identity and effective controls. Matching catalog and post-run
/// `/api/ps` digests are observational evidence, not atomic attestation of the response artifact:
/// an external client may replace/reload a tag between inference and observation (including ABA).
/// Input text and residency lifetime are absent; unknown runner defaults never count as evidence.
pub(super) fn embedding_identity(body: &Value, response: &Value, receipt: &Value) -> Value {
    let placement = &receipt["observation"];
    let Some(digest) = present(&placement["digest"]).filter(|digest| {
        placement["resident"] == true && present(&receipt["model_digest"]) == Some(*digest)
    }) else {
        return json!({"status":"unverified", "reason":"observed_model_digest_unknown_or_changed"});
    };
    let Some(vectors) = response["embeddings"]
        .as_array()
        .filter(|vectors| !vectors.is_empty())
    else {
        return json!({"status":"unverified", "reason":"embedding_vectors_missing"});
    };
    let dimensions = vectors[0].as_array().map_or(0, Vec::len);
    if dimensions == 0
        || !vectors.iter().all(|vector| {
            vector.as_array().is_some_and(|values| {
                values.len() == dimensions
                    && values
                        .iter()
                        .all(|value| value.as_f64().is_some_and(f64::is_finite))
            })
        })
    {
        return json!({"status":"unverified", "reason":"embedding_dimensions_invalid"});
    }
    let observed_context = placement["context_length"]
        .as_u64()
        .filter(|tokens| *tokens > 0);
    let explicit_context = body["options"]["num_ctx"]
        .as_u64()
        .filter(|tokens| *tokens > 0);
    if observed_context.is_none() && explicit_context.is_none() {
        return json!({"status":"unverified", "reason":"effective_context_unknown"});
    }
    json!({
        "status": "verified",
        "provenance": "catalog_and_ollama_api_ps_after_execution",
        "scope": "observed_runner_identity_non_atomic",
        "atomic_response_attestation": false,
        "model_digest": digest,
        "backend": receipt["backend"],
        "upstream": receipt["upstream"],
        "effective_options": {
            "options": body.get("options").filter(|value| value.is_object()).cloned().unwrap_or_else(|| json!({})),
            "truncate": body["truncate"].as_bool().unwrap_or(true),
            "dimensions": body.get("dimensions").cloned().unwrap_or(Value::Null),
            "observed_context_tokens": observed_context,
        },
        "dimensions": dimensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt() -> Value {
        json!({"model_digest":"sha256:original", "backend":"primary", "upstream":"http://local:11434",
            "expected_identity":{"model":"embed:latest", "model_digest":"sha256:original", "backend":"primary", "upstream":"http://local:11434"},
            "observation":{"resident":true, "digest":"sha256:original", "context_length":2048}})
    }

    #[test]
    fn backend_only_contract_needs_no_artifact_observation() {
        let backend_only = json!({"expected_identity":{
            "model":"embed:latest", "backend":"primary", "upstream":"http://local:11434"
        }});
        assert!(
            verify_selection(
                &backend_only,
                "embed:latest",
                "primary",
                "http://local:11434"
            )
            .is_ok()
        );
        assert!(verify_completed(&backend_only, &json!({})).is_ok());
        assert!(
            verify_selection(&backend_only, "embed:latest", "cpu", "http://local:11434").is_err()
        );
        assert!(
            verify_selection(
                &backend_only,
                "embed:latest",
                "primary",
                "http://different:11434"
            )
            .is_err()
        );
    }

    #[test]
    fn pinned_receipt_retains_quality_only_for_the_observed_measured_artifact() {
        use crate::platform::{CatalogModel, RouteInput, SessionAffinity, TaskKind, select_route};
        let model: CatalogModel = serde_json::from_value(json!({
            "name":"embed:latest", "digest":"sha256:original", "size":1024,
            "capabilities":["embedding"], "resident":false,
            "benchmark":{"embedding":42.0}, "policy_rank":{"embedding":0}
        }))
        .unwrap();
        let route = select_route(
            &RouteInput {
                task: TaskKind::Embedding,
                model: Some("embed:latest".to_owned()),
                ..RouteInput::default()
            },
            &[model],
            &SessionAffinity::default(),
        )
        .unwrap();
        assert_eq!(route.confidence, "medium");
        let mut verified = route.clone();
        let mut evidence = receipt();
        let observed = evidence["observation"].clone();
        finalize_route_evidence(&mut verified, &mut evidence, &observed);
        assert_eq!(verified.quality_evidence, "strong");
        for observation in [
            json!({"resident":true,"digest":"sha256:replacement"}),
            json!({"resident":true}),
        ] {
            let mut pinned = route.clone();
            let mut evidence = receipt();
            finalize_route_evidence(&mut pinned, &mut evidence, &observation);
            assert_eq!(pinned.selected_model, "embed:latest");
            assert_eq!(pinned.quality_evidence, "none");
            assert_eq!(pinned.task_evidence, "none");
            assert_eq!(pinned.confidence, "low");
            assert_eq!(evidence["qualification_identity"]["status"], "unverified");
        }
    }

    #[test]
    fn tag_replacement_is_rejected_before_inference() {
        let tags = json!({"models":[{"name":"embed:latest","digest":"sha256:replacement"}]});
        assert!(
            verify_tags(
                &tags,
                &receipt(),
                "embed:latest",
                "primary",
                "http://local:11434"
            )
            .is_err()
        );
        let tags = json!({"models":[{"name":"embed:latest","digest":"sha256:original"}]});
        assert!(
            verify_tags(
                &tags,
                &receipt(),
                "embed:latest",
                "primary",
                "http://local:11434"
            )
            .is_ok()
        );
        assert!(verify_tags(&tags, &receipt(), "embed:latest", "cpu", "http://cpu:11435").is_err());
    }

    #[test]
    fn stale_catalog_and_unknown_post_run_identity_fail_closed() {
        let mut receipt = receipt();
        let tags = json!({"models":[{"name":"embed:latest","digest":"sha256:original"}]});
        receipt["model_digest"] = json!("sha256:stale");
        assert!(
            verify_tags(
                &tags,
                &receipt,
                "embed:latest",
                "primary",
                "http://local:11434"
            )
            .is_err()
        );
        assert!(verify_completed(&receipt, &json!({"resident":true})).is_err());
        assert!(
            verify_completed(
                &receipt,
                &json!({"resident":true,"digest":"sha256:replacement"})
            )
            .is_err()
        );
        assert!(verify_completed(&receipt, &receipt["observation"]).is_ok());
    }

    #[test]
    fn embedding_identity_excludes_text_but_binds_options_dimensions_and_observation() {
        let body = json!({"input":["first"],"options":{"num_ctx":2048},"truncate":false});
        let response = json!({"embeddings":[[0.1,0.2],[0.2,0.3]]});
        let identity = embedding_identity(&body, &response, &receipt());
        assert_eq!(identity["status"], "verified");
        assert_eq!(
            identity["provenance"],
            "catalog_and_ollama_api_ps_after_execution"
        );
        assert_eq!(identity["scope"], "observed_runner_identity_non_atomic");
        assert_eq!(identity["atomic_response_attestation"], false);
        assert_eq!(identity["dimensions"], 2);
        let mut changed = body.clone();
        changed["input"] = json!(["second"]);
        assert_eq!(
            identity,
            embedding_identity(&changed, &response, &receipt())
        );
        changed["options"]["num_ctx"] = json!(4096);
        assert_ne!(
            identity,
            embedding_identity(&changed, &response, &receipt())
        );
        assert_eq!(
            embedding_identity(&body, &json!({"embeddings":[[1.0],[1.0,2.0]]}), &receipt())["status"],
            "unverified"
        );
        let mut unknown = receipt();
        unknown["observation"]["context_length"] = Value::Null;
        assert_eq!(
            embedding_identity(&json!({}), &response, &unknown)["reason"],
            "effective_context_unknown"
        );
    }
}
