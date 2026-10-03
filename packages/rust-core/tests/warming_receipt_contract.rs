use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tower::ServiceExt;
mod common;

struct Fixture {
    platform: Router,
    resident: Arc<AtomicBool>,
    loads: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(load_response: Value, ps_response: Result<Value, StatusCode>) -> Fixture {
    let resident = Arc::new(AtomicBool::new(false));
    let loads = Arc::new(AtomicUsize::new(0));
    let ps = resident.clone();
    let load = resident.clone();
    let unload = resident.clone();
    let calls = loads.clone();
    let upstream = Router::new()
        .route("/api/tags", get(|| async {
            Json(json!({"models":[{"name":"warm:receipt","digest":"warm-digest","size":100}]}))
        }))
        .route("/api/show", post(|| async {
            Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":8192}}))
        }))
        .route("/api/ps", get(move || {
            let ps = ps.clone();
            let response = ps_response.clone();
            async move {
                if !ps.load(Ordering::SeqCst) {
                    return (StatusCode::OK, Json(json!({"models":[]})));
                }
                match response {
                    Ok(value) => (StatusCode::OK, Json(value)),
                    Err(status) => (status, Json(json!({"error":"observation unavailable"}))),
                }
            }
        }))
        .route("/api/chat", post(move |Json(body): Json<Value>| {
            let load = load.clone();
            let calls = calls.clone();
            let response = load_response.clone();
            async move {
                assert_eq!(body["model"], "warm:receipt");
                assert_eq!(body["messages"], json!([]));
                calls.fetch_add(1, Ordering::SeqCst);
                load.store(true, Ordering::SeqCst);
                Json(response)
            }
        }))
        .route("/api/generate", post(move |Json(body): Json<Value>| {
            let unload = unload.clone();
            async move {
                assert_eq!(body["model"], "warm:receipt");
                assert_eq!(body["keep_alive"], 0);
                unload.store(false, Ordering::SeqCst);
                Json(json!({"done":true}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        endpoint,
        None,
        None,
        "intent:separate",
    ))
    .unwrap();
    Fixture {
        platform,
        resident,
        loads,
        server,
    }
}

async fn warm(fixture: &Fixture, keep_alive: &str) -> (StatusCode, Value) {
    let response = fixture
        .platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/warm")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"warm:receipt","keep_alive":keep_alive}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn cpu_resident() -> Value {
    json!({"models":[
        {"name":"other:resident","digest":"other-digest","size":200,"size_vram":200},
        {"name":"warm:receipt","digest":"warm-digest","size":100,"size_vram":0,"context_length":8192}
    ]})
}

#[tokio::test]
async fn warm_rejects_missing_and_nonboolean_completion_without_replaying_inference() {
    for response in [
        json!({"message":{"role":"assistant","content":""}}),
        json!({"done":null}),
        json!({"done":"true"}),
        json!({"done":1}),
        json!({"done":{}}),
        json!({"done":[]}),
        json!({"done":false}),
    ] {
        let fixture = fixture(response.clone(), Ok(cpu_resident())).await;
        let (status, result) = warm(&fixture, "0").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{response}: {result}");
        assert_eq!(result["code"], "upstream_incomplete_response", "{result}");
        assert_eq!(result["upstream_response"], response);
        assert_eq!(result["lifecycle"]["status"], "verified", "{result}");
        assert!(!fixture.resident.load(Ordering::SeqCst));
        assert_eq!(fixture.loads.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn warm_preserves_reported_upstream_error_and_requested_unload() {
    let response = json!({"error":"mock load failure"});
    let fixture = fixture(response.clone(), Ok(cpu_resident())).await;
    let (status, result) = warm(&fixture, "0").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{result}");
    assert_eq!(result["code"], "upstream_error", "{result}");
    assert_eq!(result["upstream_response"], response);
    assert_eq!(result["lifecycle"]["status"], "verified");
    assert!(!fixture.resident.load(Ordering::SeqCst));
    assert_eq!(fixture.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn warm_residency_is_independent_of_processor_verification() {
    let fixture = fixture(
        json!({"done":true,"done_reason":"load"}),
        Ok(cpu_resident()),
    )
    .await;
    let (status, result) = warm(&fixture, "30s").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["digest"], "warm-digest");
    assert_eq!(result["execution"]["observation"]["processor"], "cpu");
    assert_eq!(result["execution"]["observation"]["status"], "mismatch");
    assert_eq!(result["execution"]["observation"]["resident"], true);
    assert_eq!(result["warm"]["loaded"], true);
    assert_eq!(result["feedback"]["accepted"], false);
    assert!(fixture.resident.load(Ordering::SeqCst));
}

#[tokio::test]
async fn warm_residency_remains_known_when_processor_fields_are_missing() {
    let fixture = fixture(
        json!({"done":true,"done_reason":"load"}),
        Ok(json!({
            "models":[{"name":"warm:receipt","digest":"warm-digest"}]
        })),
    )
    .await;
    let (status, result) = warm(&fixture, "30s").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["status"], "unavailable");
    assert_eq!(result["execution"]["observation"]["resident"], true);
    assert_eq!(result["warm"]["loaded"], true);
    assert_eq!(result["feedback"]["accepted"], false);
}

#[tokio::test]
async fn unavailable_residency_observation_does_not_claim_unloaded() {
    for ps_response in [Err(StatusCode::SERVICE_UNAVAILABLE), Ok(json!({}))] {
        let fixture = fixture(json!({"done":true,"done_reason":"load"}), ps_response).await;
        let (status, result) = warm(&fixture, "30s").await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["execution"]["observation"]["status"], "unavailable");
        assert_eq!(result["execution"]["observation"]["resident"], Value::Null);
        assert_eq!(result["warm"]["loaded"], Value::Null);
        assert_eq!(result["warm"]["load_response_validated"], true);
        assert!(fixture.resident.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn immediate_unload_reports_absence_after_a_mismatched_load() {
    let fixture = fixture(
        json!({"done":true,"done_reason":"load"}),
        Ok(cpu_resident()),
    )
    .await;
    let (status, result) = warm(&fixture, "0").await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["resident"], true);
    assert_eq!(result["execution"]["observation"]["status"], "mismatch");
    assert_eq!(
        result["execution"]["lifecycle"]["post_unload_observation"]["resident"],
        false
    );
    assert_eq!(result["warm"]["loaded"], false);
    assert_eq!(
        result["warm"]["residency_source"],
        "ollama_api_ps_after_unload"
    );
    assert!(!fixture.resident.load(Ordering::SeqCst));
}
