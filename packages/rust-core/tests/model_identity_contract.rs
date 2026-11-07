//! Strict identity continuations use only a mock Ollama endpoint; no installed model is touched.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Mock {
    mode: &'static str,
    tags: AtomicUsize,
    embeds: AtomicUsize,
}

async fn identity_request(mode: &'static str) -> (StatusCode, Value, usize) {
    let counters = Arc::new(Mock {
        mode,
        tags: AtomicUsize::new(0),
        embeds: AtomicUsize::new(0),
    });
    let mock = Router::new()
        .route("/api/tags", get(|State(state): State<Arc<Mock>>| async move {
            // Discovery reads the tags twice: once to list, once to confirm that /api/show described
            // the same revision. Anything from the third read on happens after discovery.
            // The `*_at_confirmation*` modes break exactly that second read, inside discovery.
            let previous = state.tags.fetch_add(1, Ordering::SeqCst);
            let outage_from = match state.mode {
                "backend_only" => 2,
                "backend_only_confirmation_outage" => 1,
                _ => usize::MAX,
            };
            if previous >= outage_from {
                return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"metadata unavailable after discovery"})));
            }
            let change_from = match state.mode {
                "changed_at_confirmation" => 1,
                mode if mode.ends_with("changed_before") => 2,
                _ => usize::MAX,
            };
            let digest = if previous >= change_from { "sha256:replacement" } else { "sha256:original" };
            (StatusCode::OK, Json(json!({"models":[{"name":"embed:latest","digest":digest,"size":1024}]})))
        }))
        .route("/api/ps", get(|State(state): State<Arc<Mock>>| async move {
            if state.embeds.load(Ordering::SeqCst) == 0 {
                Json(json!({"models":[]}))
            } else {
                let digest = if state.mode.ends_with("unknown_after") { None } else if state.mode.ends_with("changed_after") { Some("sha256:replacement") } else { Some("sha256:original") };
                Json(json!({"models":[{"name":"embed:latest","digest":digest,"size":1024,"size_vram":1024,"context_length":2048}]}))
            }
        }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["embedding"],"model_info":{"test.context_length":8192}})) }))
        .route("/api/embed", post(|State(state): State<Arc<Mock>>| async move {
            state.embeds.fetch_add(1, Ordering::SeqCst);
            Json(json!({"model":"embed:latest","embeddings":[[0.1,0.2]],"prompt_eval_count":1}))
        }))
        .with_state(Arc::clone(&counters));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let serving = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let automatic = mode.starts_with("auto_");
    let pinned_policy = mode.starts_with("pinned_");
    let policy = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(policy.path(), "schema_version=1\n[policies.embedding]\nqualified_models=['embed:latest']\nmodel_digests={'embed:latest'='sha256:original'}\n").unwrap();
    let platform = app(&crate::support::platform_config(
        "127.0.0.1:11435",
        &upstream,
        None,
        (automatic || pinned_policy).then(|| policy.path().to_path_buf()),
    ))
    .unwrap();
    let mut request = json!({"task":"embedding", "input":["a"], "context_tokens":2048});
    if !automatic {
        request["model"] = json!("embed:latest");
    }
    if !automatic && !pinned_policy {
        if !mode.starts_with("backend_only") {
            request["expected_model_digest"] = json!("sha256:original");
        }
        request["expected_execution_backend"] = json!(if mode == "wrong_backend" {
            "cpu"
        } else {
            "primary"
        });
        request["expected_upstream"] = json!(upstream);
    }
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let embeds = counters.embeds.load(Ordering::SeqCst);
    serving.abort();
    (status, body, embeds)
}

#[tokio::test]
async fn changed_tag_and_backend_are_refused_before_any_embedding_inference() {
    for mode in [
        "changed_before",
        "changed_at_confirmation",
        "wrong_backend",
        "auto_changed_before",
    ] {
        let (status, body, calls) = identity_request(mode).await;
        assert_eq!(status, StatusCode::CONFLICT, "{mode}: {body}");
        assert_eq!(body["code"], "model_identity_changed", "{body}");
        assert_eq!(calls, 0, "{mode} must not reach /api/embed");
    }
}

#[tokio::test]
async fn replacement_observed_after_inference_never_returns_successful_vectors() {
    for mode in ["changed_after", "auto_changed_after", "auto_unknown_after"] {
        let (status, body, calls) = identity_request(mode).await;
        assert_eq!(status, StatusCode::CONFLICT, "{mode}: {body}");
        assert_eq!(body["code"], "model_identity_changed", "{body}");
        assert_eq!(calls, 1);
        assert!(
            body.get("response").is_none(),
            "changed-artifact vectors must be discarded"
        );
    }
}

#[tokio::test]
async fn explicit_operator_pin_runs_but_clears_unobserved_quality_evidence() {
    for mode in ["pinned_changed_after", "pinned_unknown_after"] {
        let (status, body, calls) = identity_request(mode).await;
        assert_eq!(status, StatusCode::OK, "{mode}: {body}");
        assert_eq!(calls, 1);
        assert_eq!(body["route"]["quality_evidence"], "none");
        assert_eq!(body["route"]["evidence"], "capability_metadata_only");
        assert_eq!(
            body["execution"]["qualification_identity"]["status"],
            "unverified"
        );
    }
}

#[tokio::test]
async fn backend_only_contract_ignores_unneeded_metadata_outage_after_discovery() {
    // An outage after discovery, and one at discovery's own tags confirmation read, must both
    // leave a contract that asserts no artifact digest runnable.
    for mode in ["backend_only", "backend_only_confirmation_outage"] {
        let (status, body, calls) = identity_request(mode).await;
        assert_eq!(status, StatusCode::OK, "{mode}: {body}");
        assert_eq!(calls, 1);
        assert_eq!(body["execution"]["backend"], "primary");
        assert!(body["execution"]["expected_identity"]["model_digest"].is_null());
    }
}

#[tokio::test]
async fn unchanged_embedding_returns_observed_identity_and_effective_options() {
    let (status, body, calls) = identity_request("unchanged").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(calls, 1);
    assert_eq!(body["embedding_identity"]["status"], "verified");
    assert_eq!(
        body["embedding_identity"]["model_digest"],
        "sha256:original"
    );
    assert_eq!(body["embedding_identity"]["backend"], "primary");
    assert_eq!(body["embedding_identity"]["dimensions"], 2);
    assert_eq!(
        body["embedding_identity"]["effective_options"]["truncate"],
        false
    );
    assert_eq!(
        body["embedding_identity"]["effective_options"]["observed_context_tokens"],
        2048
    );
}
