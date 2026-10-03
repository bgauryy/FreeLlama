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
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Mutex;
use tower::ServiceExt;
mod common;

struct Backend {
    endpoint: String,
    resident: Arc<Mutex<Vec<Value>>>,
    chats: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn backend(name: &str, file_size: u64, resident: Vec<Value>) -> Backend {
    let resident = Arc::new(Mutex::new(resident));
    let chats = Arc::new(AtomicUsize::new(0));
    let entries = resident.clone();
    let calls = chats.clone();
    let name = name.to_owned();
    let tags = json!({"models":[{"name":name,"digest":format!("{name}-digest"),"size":file_size}]});
    let upstream = Router::new()
        .route("/api/tags", get(move || {
            let tags = tags.clone();
            async move { Json(tags) }
        }))
        .route("/api/ps", get(move || {
            let entries = entries.clone();
            async move { Json(json!({"models":entries.lock().await.clone()})) }
        }))
        .route("/api/show", post(|| async {
            Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":8192}}))
        }))
        .route("/api/chat", post(move |Json(body): Json<Value>| {
            let calls = calls.clone();
            let name = name.clone();
            async move {
                assert_eq!(body["model"], name);
                if name == "cpu:profile" {
                    assert_eq!(body["options"]["num_gpu"], 0);
                }
                calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({"done":true,"message":{"role":"assistant","content":"ok"}}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    Backend {
        endpoint,
        resident,
        chats,
        server,
    }
}

fn running(name: &str, size: u64, vram: u64) -> Value {
    json!({"name":name,"digest":format!("{name}-digest"),"size":size,"size_vram":vram,"context_length":4096})
}

async fn request(platform: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = platform
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/_freellama/v1/{path}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn model<'a>(inventory: &'a Value, name: &str) -> &'a Value {
    inventory["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn resident_bytes_drive_gpu_inventory_preview_and_scoped_execution() {
    let backend = backend(
        "gpu:profile",
        4000,
        vec![running("gpu:profile", 1000, 1000)],
    )
    .await;
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        backend.endpoint.clone(),
        None,
        None,
        "intent:separate",
    ))
    .unwrap();
    let (status, inventory) = request(&platform, "GET", "models", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let entry = model(&inventory, "gpu:profile");
    assert_eq!(entry["size"], 4000);
    assert_eq!(entry["execution"]["observation"]["size"], 1000);
    assert_eq!(entry["resident_size"], 1000);
    assert_eq!(entry["execution"]["observation"]["status"], "verified");
    let route =
        json!({"model":"gpu:profile","context_tokens":4096,"min_placement_evidence":"observed"});
    let (status, preview) = request(&platform, "POST", "routes", route.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let (_, scope) = request(&platform, "POST", "scopes", json!({})).await;
    let mut task = route;
    task["prompt"] = json!("next");
    task["scope_id"] = scope["scope_id"].clone();
    task["scope_revision"] = json!(0);
    let (status, result) = request(&platform, "POST", "tasks", task).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["size"], 1000);
    assert_eq!(result["execution"]["observation"]["status"], "verified");
    assert_eq!(
        result["execution"]["memory_kv_preflight"]["known_model_bytes"],
        4000
    );
    assert_eq!(result["scope"]["revision"], 1);
    assert_eq!(backend.chats.load(Ordering::SeqCst), 1);

    *backend.resident.lock().await = vec![running("gpu:profile", 2000, 2000)];
    let (_, inventory) = request(&platform, "GET", "models", Value::Null).await;
    let entry = model(&inventory, "gpu:profile");
    assert_eq!(entry["resident_size"], 2000);
    assert_eq!(entry["execution"]["observation"]["size"], 2000);
    assert_eq!(entry["size"], 4000);
    assert_eq!(entry["execution"]["observation"]["status"], "verified");

    *backend.resident.lock().await = vec![running("gpu:profile", 2000, 500)];
    let (_, inventory) = request(&platform, "GET", "models", Value::Null).await;
    assert_eq!(
        model(&inventory, "gpu:profile")["execution"]["observation"]["processor"],
        "mixed"
    );
    let (status, refusal) = request(
        &platform,
        "POST",
        "routes",
        json!({
            "model":"gpu:profile","min_placement_evidence":"observed"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("observed=mixed")
    );
    assert_eq!(backend.chats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn positive_vram_without_runner_total_never_verifies_gpu_or_trains_feedback() {
    let backend = backend(
        "gpu:profile",
        4000,
        vec![json!({
            "name":"gpu:profile","digest":"gpu:profile-digest","size_vram":500,"context_length":4096
        })],
    )
    .await;
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        backend.endpoint.clone(),
        None,
        None,
        "intent:separate",
    ))
    .unwrap();
    let (_, inventory) = request(&platform, "GET", "models", Value::Null).await;
    let entry = model(&inventory, "gpu:profile");
    assert_eq!(entry["resident_size"], Value::Null);
    assert_eq!(entry["execution"]["observation"]["processor"], "unknown");
    assert_eq!(entry["execution"]["observation"]["status"], "unavailable");
    let route = json!({"model":"gpu:profile","min_placement_evidence":"observed"});
    let (status, refusal) = request(&platform, "POST", "routes", route.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("observed=unknown")
    );
    let mut task = route;
    task["prompt"] = json!("uncertain placement");
    let (status, refusal) = request(&platform, "POST", "tasks", task.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        refusal["error"]
            .as_str()
            .unwrap()
            .contains("observed=unknown")
    );
    assert_eq!(backend.chats.load(Ordering::SeqCst), 0);
    task["min_placement_evidence"] = json!("configured");
    let (status, result) = request(&platform, "POST", "tasks", task).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["processor"], "unknown");
    assert_eq!(result["execution"]["observation"]["status"], "unavailable");
    assert_eq!(result["execution"]["observation"]["resident"], true);
    assert_eq!(result["feedback"]["accepted"], false);
    assert_eq!(backend.chats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cpu_and_unknown_runner_evidence_preserve_observed_gates() {
    let primary = backend("gpu:profile", 4000, Vec::new()).await;
    let cpu = backend("cpu:profile", 6000, vec![running("cpu:profile", 2000, 0)]).await;
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        primary.endpoint.clone(),
        None,
        None,
        "intent:separate",
    )
    .with_cpu_backend(cpu.endpoint.clone(), ["cpu:profile"]))
    .unwrap();
    let (_, inventory) = request(&platform, "GET", "models", Value::Null).await;
    let entry = model(&inventory, "cpu:profile");
    assert_eq!(entry["size"], 6000);
    assert_eq!(entry["execution"]["observation"]["size"], 2000);
    assert_eq!(entry["execution"]["observation"]["processor"], "cpu");
    assert_eq!(entry["execution"]["observation"]["status"], "verified");
    let route = json!({"model":"cpu:profile","min_placement_evidence":"observed"});
    let (status, preview) = request(&platform, "POST", "routes", route.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let mut task = route.clone();
    task["prompt"] = json!("cpu task");
    let (status, result) = request(&platform, "POST", "tasks", task).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["processor"], "cpu");
    assert_eq!(result["execution"]["observation"]["status"], "verified");
    assert_eq!(cpu.chats.load(Ordering::SeqCst), 1);
    assert_eq!(primary.chats.load(Ordering::SeqCst), 0);

    for residents in [vec![json!({"name":"cpu:profile","size":2000})], Vec::new()] {
        let expected_size = residents
            .first()
            .and_then(|entry| entry.get("size"))
            .cloned()
            .unwrap_or(Value::Null);
        *cpu.resident.lock().await = residents;
        let (_, inventory) = request(&platform, "GET", "models", Value::Null).await;
        let entry = model(&inventory, "cpu:profile");
        assert_eq!(entry["resident_size"], expected_size);
        assert_eq!(entry["execution"]["observation"]["size"], expected_size);
        assert_eq!(entry["execution"]["observation"]["processor"], "unknown");
        let (status, refusal) = request(&platform, "POST", "routes", route.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            refusal["error"]
                .as_str()
                .unwrap()
                .contains("observed=unknown")
        );
        assert_eq!(cpu.chats.load(Ordering::SeqCst), 1);
    }
}
