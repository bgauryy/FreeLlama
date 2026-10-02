//! Memory-aware placement contracts: resident runners under pressure, context reuse, discrete-GPU
//! spill accounting and idle-model eviction.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Mock {
    /// Models reported by /api/ps.
    loaded: Arc<Mutex<Vec<Value>>>,
    /// Bodies sent to /api/chat.
    chats: Arc<Mutex<Vec<Value>>>,
    /// Set when /api/generate unloads a model.
    unloaded: Arc<AtomicBool>,
}

fn policy() -> ResourcePolicy {
    ResourcePolicy {
        sample_interval: Duration::from_millis(5),
        hold_available_percent: 1,
        resume_available_percent: 2,
        hold_available_min_bytes: 100,
        resume_available_min_bytes: 200,
        ..ResourcePolicy::default()
    }
}

fn sample(available: u64, gpu_free: Option<u64>) -> HostResources {
    HostResources {
        source: "injected_contract".into(),
        total_memory_bytes: Some(10_000),
        available_memory_bytes: Some(available),
        memory_pressure: Some(MemoryPressure::Normal),
        thermal_throttled: Some(false),
        gpu_memory_total_bytes: gpu_free.map(|_| 12_000),
        gpu_memory_free_bytes: gpu_free,
        gpu_telemetry_source: gpu_free.map(|_| "injected".into()),
        ..HostResources::default()
    }
}

async fn backend(mock: Mock) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [
                    {"name": "big:latest", "digest": "d-big", "size": 8000},
                    {"name": "idle:latest", "digest": "d-idle", "size": 3000},
                    {"name": "huge:latest", "digest": "d-huge", "size": 20000}
                ]}))
            }),
        )
        .route(
            "/api/ps",
            get(|State(mock): State<Mock>| async move {
                Json(json!({"models": mock.loaded.lock().await.clone()}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({"capabilities": ["completion"],
                    "model_info": {"general.architecture": "llama", "llama.context_length": 32768}}))
            }),
        )
        .route(
            "/api/generate",
            post(|State(mock): State<Mock>, Json(body): Json<Value>| async move {
                if body["keep_alive"] == 0 {
                    mock.loaded
                        .lock()
                        .await
                        .retain(|entry| entry["name"] != body["model"]);
                    mock.unloaded.store(true, Ordering::SeqCst);
                }
                Json(json!({"done": true}))
            }),
        )
        .route(
            "/api/chat",
            post(|State(mock): State<Mock>, Json(body): Json<Value>| async move {
                mock.chats.lock().await.push(body);
                Json(json!({"message": {"role": "assistant", "content": "ok"}, "done": true}))
            }),
        )
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}

async fn run(platform: Router, model: &str, prompt: &str) -> (StatusCode, Value) {
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"task": "completion", "objective": "fastest", "model": model,
                        "prompt": prompt, "request_options": {"options": {"num_predict": 64}}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
        .unwrap_or(Value::Null);
    (status, body)
}

fn config(upstream: String, governor: ResourceGovernor) -> PlatformConfig {
    let mut config = PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "idle:latest")
        .with_max_queue_wait(Duration::from_millis(1500));
    config.resource_governor = governor;
    config
}

#[tokio::test]
async fn resident_runner_is_served_under_a_low_memory_hold() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "big:latest", "digest": "d-big", "size": 8000, "size_vram": 8000,
        "context_length": 4096, "expires_at": "2026-01-01T00:05:00Z"
    }));
    let (upstream, server) = backend(mock.clone()).await;
    // Available memory is below the hold reserve: the loaded model itself is what used it.
    let governor = ResourceGovernor::with_sampler(policy(), || sample(50, Some(0))).unwrap();
    let platform = app(&config(upstream, governor)).unwrap();
    let preview = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/routes")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"task":"completion","objective":"fastest",
                "model":"big:latest","context_tokens":4096})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(preview.status(), StatusCode::OK);
    let preview: Value =
        serde_json::from_slice(&to_bytes(preview.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        preview["execution"]["agent_plan"]["dispatch_readiness"], "runnable_now",
        "{preview}"
    );
    let (status, body) = run(platform, "big:latest", "hi").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["execution"]["memory_reservation"]["source"],
        "matching_resident_context"
    );
    server.abort();
}

#[tokio::test]
async fn resident_runner_context_is_reused_instead_of_reloading() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "big:latest", "digest": "d-big", "size": 8000, "size_vram": 8000,
        "context_length": 16384, "expires_at": "2026-01-01T00:05:00Z"
    }));
    let (upstream, server) = backend(mock.clone()).await;
    let governor = ResourceGovernor::with_sampler(policy(), || sample(5000, Some(0))).unwrap();
    let (status, body) = run(
        app(&config(upstream, governor)).unwrap(),
        "big:latest",
        "hi",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A short prompt alone would size to 2048; asking for that would reload the 16k runner.
    assert_eq!(mock.chats.lock().await[0]["options"]["num_ctx"], 16384);
    assert_eq!(
        body["execution"]["context_sizing"]["reused_resident_context"],
        16384
    );
    server.abort();
}

// Apple Silicon has unified memory, where discrete-GPU spill accounting does not apply.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[tokio::test]
async fn discrete_gpu_spill_is_reserved_and_idle_models_are_evicted_to_fit() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "idle:latest", "digest": "d-idle", "size": 3000, "size_vram": 3000,
        "context_length": 2048, "expires_at": "2026-01-01T00:05:00Z"
    }));
    let (upstream, server) = backend(mock.clone()).await;
    let unloaded = Arc::clone(&mock.unloaded);
    let host_available = Arc::new(std::sync::atomic::AtomicU64::new(2000));
    let governor = ResourceGovernor::with_sampler(policy(), move || {
        // 8000-byte model estimates 10800 with assumed KV and graph margin. With the idle model
        // loaded only 1000 bytes of VRAM are free, so 9800 would spill into 2000 bytes of RAM.
        // Unloading it leaves all but 1000 bytes in VRAM.
        let gpu_free = if unloaded.load(Ordering::SeqCst) {
            9800
        } else {
            1000
        };
        sample(host_available.load(Ordering::SeqCst), Some(gpu_free))
    })
    .unwrap();
    let (status, body) = run(
        app(&config(upstream, governor)).unwrap(),
        "big:latest",
        "hi",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reservation = &body["execution"]["memory_reservation"];
    assert_eq!(reservation["source"], "discrete_gpu_spill_estimate");
    assert_eq!(reservation["evicted_idle_models"], json!(["idle:latest"]));
    assert_eq!(reservation["required_available_bytes"], 1000);
    server.abort();
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[tokio::test]
async fn pinned_models_are_never_evicted() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "idle:latest", "digest": "d-idle", "size": 3000, "size_vram": 3000,
        "context_length": 2048, "expires_at": "2318-01-01T00:00:00Z"
    }));
    let (upstream, server) = backend(mock.clone()).await;
    let governor = ResourceGovernor::with_sampler(policy(), || sample(2000, Some(1000))).unwrap();
    let (status, _) = run(
        app(&config(upstream, governor)).unwrap(),
        "big:latest",
        "hi",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!mock.unloaded.load(Ordering::SeqCst));
    assert_eq!(mock.loaded.lock().await.len(), 1);
    server.abort();
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[tokio::test]
async fn a_model_larger_than_the_gpu_does_not_empty_vram() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "idle:latest", "digest": "d-idle", "size": 3000, "size_vram": 3000,
        "context_length": 2048, "expires_at": "2026-01-01T00:05:00Z"
    }));
    let (upstream, server) = backend(mock.clone()).await;
    // 20000-byte model on a 12000-byte GPU: it spills whatever is unloaded.
    let governor = ResourceGovernor::with_sampler(policy(), || sample(9000, Some(9000))).unwrap();
    let _ = run(
        app(&config(upstream, governor)).unwrap(),
        "huge:latest",
        "hi",
    )
    .await;
    assert!(!mock.unloaded.load(Ordering::SeqCst));
    server.abort();
}
