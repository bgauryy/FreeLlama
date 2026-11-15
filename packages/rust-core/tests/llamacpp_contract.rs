//! Managed llama.cpp contracts against loopback HTTP fixtures; no model or Ollama is needed.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use freellama::{
    platform::{
        self, Backend, PlatformConfig,
        resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
    },
    proxy::{LlamaCppConfig, OpenAiUpstream},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

type RecordedCalls = Arc<Mutex<Vec<(Value, Option<String>)>>>;

#[derive(Clone, Default)]
struct Mock {
    calls: RecordedCalls,
    mode: Arc<AtomicUsize>,
    observations: Arc<AtomicUsize>,
}

async fn fixture(
    memory: u64,
    units: usize,
) -> (Router, Mock, ResourceGovernor, tokio::task::JoinHandle<()>) {
    fixture_with_memory(
        Arc::new(AtomicU64::new(memory)),
        units,
        Duration::from_millis(100),
    )
    .await
}

async fn fixture_with_memory(
    memory: Arc<AtomicU64>,
    units: usize,
    wait: Duration,
) -> (Router, Mock, ResourceGovernor, tokio::task::JoinHandle<()>) {
    let mock = Mock::default();
    let server = Router::new()
        .route("/v1/health", get(|State(mock): State<Mock>| async move {
            mock.observations.fetch_add(1, Ordering::SeqCst);
            Json(json!({"status": "ok"}))
        }))
        .route("/v1/models", get(|State(mock): State<Mock>| async move {
            Json(json!({"data": [{"id": if mock.mode.load(Ordering::SeqCst) == 4 { "wrong" } else { "text-model" }}]}))
        }))
        .route("/v1/chat/completions", post(|State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>| async move {
            mock.calls.lock().await.push((body, headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned)));
            let mode = mock.mode.load(Ordering::SeqCst);
            if mode == 1 { return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "after dispatch"}))); }
            if mode == 2 { tokio::time::sleep(Duration::from_secs(2)).await; }
            (StatusCode::OK, Json(json!({"choices":[{"message":{"role":"assistant", "content":"mock answer"},
                "finish_reason": if mode == 3 { Value::Null } else { json!("stop") }}],
                "usage":{"prompt_tokens": 7, "completion_tokens": 3}})))
        }))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            first_sample_grace: Duration::from_millis(20),
            ..ResourcePolicy::default()
        },
        move || HostResources {
            total_memory_bytes: Some(16 << 30),
            available_memory_bytes: Some(memory.load(Ordering::SeqCst)),
            memory_pressure: Some(MemoryPressure::Normal),
            load_average_one_minute: Some(0.0),
            logical_cpus: Some(4),
            ..HostResources::default()
        },
    )
    .unwrap();
    // Port zero is intentionally unavailable: explicit llama.cpp must not depend on Ollama.
    let mut config = PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:0", None, None)
        .with_cpu_max_concurrent_tasks(units)
        .with_max_queue_wait(wait)
        .with_cpu_max_queued_tasks(1)
        .with_cpu_backend("http://127.0.0.1:1", ["unused:latest"])
        .with_upstreams(vec![OpenAiUpstream {
            name: "cpp".into(),
            base_url: url,
            api_key: Some("engine-key".into()),
            llamacpp: Some(LlamaCppConfig {
                model: "text-model".into(),
                processor: Backend::Cpu,
                context_tokens: 4096,
                max_output_tokens: 64,
                memory_bytes: 1 << 30,
            }),
        }]);
    config.resource_governor = governor.clone();
    (platform::app(&config).unwrap(), mock, governor, handle)
}

fn task() -> Value {
    json!({"task":"completion", "model":"cpp/text-model", "prompt":"hello"})
}
async fn call(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
const TASKS: &str = "/_freellama/v1/tasks";
const ROUTE: &str = "/_freellama/v1/routes";

#[tokio::test]
async fn managed_text_preview_and_receipt_work_without_ollama_and_release_memory() {
    let (app, mock, governor, server) = fixture(8 << 30, 2).await;
    let (status, preview) = call(
        &app,
        ROUTE,
        json!({"task":"completion","model":"cpp/text-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["execution"]["preview_only"], true);
    assert_eq!(mock.calls.lock().await.len(), 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    let mut body = task();
    body["request_options"] = json!({"options":{"num_predict":12, "temperature":0.2}});
    let (status, result) = call(&app, TASKS, body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["response"]["message"]["content"], "mock answer");
    assert_eq!(result["metrics"]["output_tokens"], 3);
    assert_eq!(result["execution"]["engine"], "llamacpp");
    assert_eq!(
        result["execution"]["physical_placement"]["status"],
        "unknown"
    );
    assert_eq!(result["resources"]["reserved_bytes"], 1_u64 << 30);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    let calls = mock.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0["model"], "text-model");
    assert_eq!(calls[0].0["max_tokens"], 12);
    assert_eq!(calls[0].0["stream"], false);
    assert_eq!(calls[0].1.as_deref(), Some("Bearer engine-key"));
    server.abort();
}

#[tokio::test]
async fn unsupported_capabilities_and_unbounded_options_refuse_before_observation_or_dispatch() {
    let (app, mock, _, server) = fixture(8 << 30, 2).await;
    for (field, value) in [
        ("tools", json!([])),
        ("images", json!([])),
        ("keep_alive", json!("0")),
        ("stop_on_repetition", json!(true)),
        ("context_tokens", json!(1024)),
        ("min_placement_evidence", json!("observed")),
        ("task", json!("embedding")),
        ("task", json!("code_repair")),
        ("request_options", json!({"options":{"num_predict":-1}})),
        ("request_options", json!({"options":{"num_gpu":0}})),
        ("request_options", json!({"format":"json"})),
        (
            "messages",
            json!([{"role":"user", "content":"hi", "images":["x"]}]),
        ),
    ] {
        let mut body = task();
        body[field] = value;
        let (status, error) = call(&app, TASKS, body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "field={field}: {error}"
        );
    }
    assert_eq!(mock.observations.load(Ordering::SeqCst), 0);
    assert_eq!(mock.calls.lock().await.len(), 0);
    server.abort();
}

#[tokio::test]
async fn resource_and_model_observation_failures_never_dispatch() {
    let (app, mock, governor, server) = fixture(1 << 30, 2).await;
    let (status, error) = call(&app, TASKS, task()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert_eq!(mock.calls.lock().await.len(), 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    mock.mode.store(4, Ordering::SeqCst);
    let (status, error) = call(&app, TASKS, task()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(mock.calls.lock().await.len(), 0);
    server.abort();
}

#[tokio::test]
async fn accepted_errors_and_malformed_results_are_never_retried() {
    let (app, mock, governor, server) = fixture(8 << 30, 2).await;
    for mode in [1, 3] {
        mock.mode.store(mode, Ordering::SeqCst);
        let (status, error) = call(&app, TASKS, task()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    }
    assert_eq!(mock.calls.lock().await.len(), 2);
    server.abort();
}

#[tokio::test]
async fn weighted_pool_and_total_deadline_bound_work_and_release_permits() {
    let (app, mock, governor, server) = fixture(8 << 30, 2).await;
    mock.mode.store(2, Ordering::SeqCst);
    let first = app.clone();
    let running = tokio::spawn(async move {
        let mut body = task();
        body["timeout_seconds"] = json!(1);
        call(&first, TASKS, body).await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while mock.calls.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (status, error) = call(&app, TASKS, task()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert_eq!(mock.calls.lock().await.len(), 1);
    let (status, error) = running.await.unwrap();
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{error}");
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    mock.mode.store(0, Ordering::SeqCst);
    let (status, result) = call(&app, TASKS, task()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(mock.calls.lock().await.len(), 2);
    server.abort();
}

#[tokio::test]
async fn lifecycle_requests_refuse_without_contacting_either_engine() {
    let (app, mock, _, server) = fixture(8 << 30, 2).await;
    for path in ["/_freellama/v1/warm", "/_freellama/v1/unload"] {
        let (status, error) = call(&app, path, json!({"model":"cpp/text-model"})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["code"], "engine_capability_unsupported");
    }
    assert_eq!(mock.observations.load(Ordering::SeqCst), 0);
    assert_eq!(mock.calls.lock().await.len(), 0);
    server.abort();
}

async fn admission(app: &Router) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::get("/_freellama/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["backends"]["cpu"]["admission"].clone()
}

async fn resource_waiting(app: &Router) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let observed = admission(app).await;
            if observed["resource_waiters"] == 1 {
                return observed;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn memory_wait_yields_execution_slots_keeps_a_bounded_ticket_and_cancels_cleanly() {
    let memory = Arc::new(AtomicU64::new(1 << 30));
    let (app, mock, governor, server) =
        fixture_with_memory(memory, 2, Duration::from_secs(5)).await;
    let first = app.clone();
    let pending = tokio::spawn(async move { call(&first, TASKS, task()).await });
    let observed = resource_waiting(&app).await;
    assert_eq!(observed["in_flight"], 0, "{observed}");
    assert_eq!(observed["queue_depth"], 1, "{observed}");
    assert_eq!(mock.calls.lock().await.len(), 0);
    let (status, error) = call(&app, TASKS, task()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{error}");
    pending.abort();
    let _ = pending.await;
    assert_eq!(admission(&app).await["queue_depth"], 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[tokio::test]
async fn memory_recovery_readmits_the_existing_waiter_and_releases_all_permits() {
    let memory = Arc::new(AtomicU64::new(1 << 30));
    let (app, mock, governor, server) =
        fixture_with_memory(memory.clone(), 2, Duration::from_secs(5)).await;
    let first = app.clone();
    let pending = tokio::spawn(async move { call(&first, TASKS, task()).await });
    let observed = resource_waiting(&app).await;
    assert_eq!(observed["in_flight"], 0, "{observed}");
    memory.store(8 << 30, Ordering::SeqCst);
    governor.invalidate().await;
    let (status, receipt) = pending.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(mock.calls.lock().await.len(), 1);
    assert_eq!(admission(&app).await["queue_depth"], 0);
    assert_eq!(admission(&app).await["in_flight"], 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[test]
fn managed_configuration_requires_explicit_local_bounded_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upstreams.toml");
    let valid = r#"[[upstream]]
name = "cpp"
url = "http://127.0.0.1:8080/v1"
[upstream.llamacpp]
model = "text-model"
processor = "cpu"
context_tokens = 4096
max_output_tokens = 64
memory_bytes = 1073741824
"#;
    std::fs::write(&path, valid).unwrap();
    assert!(
        freellama::proxy::load_upstreams(&path).unwrap()[0]
            .llamacpp
            .is_some()
    );
    for invalid in [
        valid.replace("127.0.0.1", "example.com"),
        valid.replace("memory_bytes = 1073741824", "memory_bytes = 0"),
        valid.replace("max_output_tokens = 64", "max_output_tokens = 4096"),
        valid.replace("processor = \"cpu\"", "processor = \"auto\""),
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(freellama::proxy::load_upstreams(&path).is_err());
    }
}
