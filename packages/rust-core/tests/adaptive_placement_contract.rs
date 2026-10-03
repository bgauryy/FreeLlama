use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    app,
    resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::Mutex;
use tower::ServiceExt;

mod common;

const MODEL: &str = "adaptive:latest";

struct Fixture {
    platform: Router,
    resident: Arc<Mutex<Value>>,
    decode_ns: Arc<AtomicU64>,
    fail: Arc<AtomicBool>,
    pressure_on_completion: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    _runtime: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn running(size: Option<u64>, vram: u64) -> Value {
    json!({"name":MODEL,"digest":"revision-a","size":size,"size_vram":vram,"context_length":4096})
}

async fn fixture() -> Fixture {
    let resident = Arc::new(Mutex::new(running(Some(1000), 1000)));
    let decode_ns = Arc::new(AtomicU64::new(1_000_000_000));
    let fail = Arc::new(AtomicBool::new(false));
    let pressure_on_completion = Arc::new(AtomicBool::new(false));
    let pressure = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_pressure = Arc::clone(&pressure);
    let governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), move || HostResources {
            source: "adaptive_placement_fixture".into(),
            total_memory_bytes: Some(1 << 50),
            available_memory_bytes: Some(1 << 49),
            memory_pressure: Some(if observed_pressure.load(Ordering::SeqCst) {
                MemoryPressure::Warning
            } else {
                MemoryPressure::Normal
            }),
            ..HostResources::default()
        })
        .unwrap();
    let entries = Arc::clone(&resident);
    let duration = Arc::clone(&decode_ns);
    let failure = Arc::clone(&fail);
    let pressure_trigger = Arc::clone(&pressure_on_completion);
    let samples = governor.clone();
    let reached = Arc::clone(&calls);
    let upstream = Router::new()
        .route("/api/tags", get(|| async {
            Json(json!({"models":[{"name":MODEL,"digest":"revision-a","size":4000}]}))
        }))
        .route("/api/show", post(|| async {
            Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":8192}}))
        }))
        .route("/api/ps", get(move || {
            let entries = Arc::clone(&entries);
            async move { Json(json!({"models":[entries.lock().await.clone()]})) }
        }))
        .route("/api/chat", post(move |Json(body): Json<Value>| {
            let duration = Arc::clone(&duration);
            let failure = Arc::clone(&failure);
            let pressure_trigger = Arc::clone(&pressure_trigger);
            let pressure = Arc::clone(&pressure);
            let samples = samples.clone();
            let reached = Arc::clone(&reached);
            async move {
                assert_eq!(body["model"], MODEL);
                reached.fetch_add(1, Ordering::SeqCst);
                if pressure_trigger.load(Ordering::SeqCst) {
                    pressure.store(true, Ordering::SeqCst);
                    samples.invalidate().await;
                }
                if failure.load(Ordering::SeqCst) {
                    return (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error":"fixture timeout"})));
                }
                (StatusCode::OK, Json(json!({
                    "done":true,"message":{"role":"assistant","content":"ok"},
                    "eval_count":100,"eval_duration":duration.load(Ordering::SeqCst)
                })))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let runtime = tempfile::tempdir().unwrap();
    let file = runtime.path().join("runtime.toml");
    std::fs::write(&file, "adaptive_concurrency = \"all\"\n").unwrap();
    let mut config = common::platform_config("127.0.0.1:11435", endpoint, None, None, MODEL)
        .with_max_concurrent_tasks(4)
        .with_runtime_config(file);
    config.resource_governor = governor;
    Fixture {
        platform: app(&config).unwrap(),
        resident,
        decode_ns,
        fail,
        pressure_on_completion,
        calls,
        _runtime: runtime,
        server,
    }
}

async fn task(fixture: &Fixture) -> (StatusCode, Value) {
    task_with(fixture, 4096, json!({})).await
}

async fn task_with(fixture: &Fixture, context: u64, options: Value) -> (StatusCode, Value) {
    let response = fixture.platform.clone().oneshot(
        Request::post("/_freellama/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(json!({
                "task":"completion","model":MODEL,"objective":"fastest",
                "context_tokens":context,"min_placement_evidence":"configured","prompt":"fixture",
                "request_options":{"options":options}
            }).to_string()))
            .unwrap(),
    ).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn changed_context_without_process_identity_skips_throughput_learning() {
    let fixture = fixture().await;
    verified_baseline(&fixture).await;
    fixture.resident.lock().await["context_length"] = json!(8192);
    fixture.decode_ns.store(10_000_000_000, Ordering::SeqCst);
    let (status, result) = task_with(&fixture, 8192, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["context_length"], 8192);
    assert_eq!(
        adaptive(&fixture).await["limit"],
        4,
        "the 4K profile baseline must not throttle a distinct 8K execution"
    );
}

#[tokio::test]
async fn changed_runner_option_without_process_identity_skips_throughput_learning() {
    let fixture = fixture().await;
    verified_baseline(&fixture).await;
    fixture.decode_ns.store(10_000_000_000, Ordering::SeqCst);
    let (status, result) = task_with(&fixture, 4096, json!({"num_thread":1})).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["runtime_options"]["num_thread"], 1);
    assert_eq!(
        adaptive(&fixture).await["limit"],
        4,
        "different runner controls must not share a throughput baseline"
    );
}

#[tokio::test]
async fn missing_digest_does_not_train_or_recover_the_limiter() {
    let fixture = fixture().await;
    verified_baseline(&fixture).await;
    fixture.fail.store(true, Ordering::SeqCst);
    assert_eq!(task(&fixture).await.0, StatusCode::GATEWAY_TIMEOUT);
    fixture.fail.store(false, Ordering::SeqCst);
    fixture
        .resident
        .lock()
        .await
        .as_object_mut()
        .unwrap()
        .remove("digest");
    for _ in 0..5 {
        let (status, result) = task(&fixture).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["execution"]["observation"]["status"], "verified");
    }
    assert_eq!(
        adaptive(&fixture).await["limit"],
        2,
        "unknown model revision must not count toward healthy recovery"
    );
}

async fn adaptive(fixture: &Fixture) -> Value {
    let response = fixture
        .platform
        .clone()
        .oneshot(
            Request::get("/_freellama/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let health: Value = serde_json::from_slice(&body).unwrap();
    let adaptive = health["backends"]["gpu"]["adaptive"].clone();
    assert!(
        adaptive.is_object(),
        "status must expose the adaptive controller: {health}"
    );
    adaptive
}

async fn verified_baseline(fixture: &Fixture) -> Value {
    for _ in 0..3 {
        let (status, result) = task(fixture).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["execution"]["observation"]["status"], "verified");
        assert_eq!(
            result["execution"]["throughput_learning"]["eligible"],
            false
        );
        assert_eq!(
            result["execution"]["throughput_learning"]["reason"],
            "backend_process_settings_unknown"
        );
    }
    let baseline = adaptive(fixture).await;
    assert_eq!(baseline["enabled"], true);
    assert_eq!(baseline["limit"], 4);
    assert_eq!(baseline["healthy_streak"], 0);
    assert_eq!(baseline["current_profile_count"], 0);
    assert_eq!(baseline["baseline_output_tokens_per_second"], json!({}));
    baseline
}

async fn excludes_slow_sample(resident: Value, processor: &str, placement_status: &str) {
    let fixture = fixture().await;
    let baseline = verified_baseline(&fixture).await;
    *fixture.resident.lock().await = resident;
    fixture.decode_ns.store(10_000_000_000, Ordering::SeqCst);
    let (status, result) = task(&fixture).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["processor"], processor);
    assert_eq!(
        result["execution"]["observation"]["status"],
        placement_status
    );
    assert_eq!(result["feedback"]["accepted"], false);
    assert_eq!(
        adaptive(&fixture).await,
        baseline,
        "unverified success must neither train nor reduce the requested backend's limiter"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn unknown_placement_does_not_reduce_the_adaptive_limit() {
    excludes_slow_sample(running(None, 500), "unknown", "unavailable").await;
}

#[tokio::test]
async fn mixed_placement_does_not_reduce_the_adaptive_limit() {
    excludes_slow_sample(running(Some(1000), 500), "mixed", "mismatch").await;
}

#[tokio::test]
async fn mismatched_cpu_placement_does_not_reduce_the_gpu_adaptive_limit() {
    excludes_slow_sample(running(Some(1000), 0), "cpu", "mismatch").await;
}

#[tokio::test]
async fn unverified_healthy_completions_do_not_train_or_recover_the_limiter() {
    let fixture = fixture().await;
    verified_baseline(&fixture).await;
    fixture.fail.store(true, Ordering::SeqCst);
    let (status, result) = task(&fixture).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{result}");
    let baseline = adaptive(&fixture).await;
    assert_eq!(baseline["limit"], 2);
    assert_eq!(baseline["healthy_streak"], 0);
    fixture.fail.store(false, Ordering::SeqCst);
    *fixture.resident.lock().await = running(None, 500);
    for _ in 0..5 {
        let (status, result) = task(&fixture).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["execution"]["observation"]["status"], "unavailable");
    }
    assert_eq!(
        adaptive(&fixture).await,
        baseline,
        "missing placement evidence must not count toward healthy recovery or a throughput baseline"
    );
}

#[tokio::test]
async fn upstream_failure_still_throttles_without_verified_placement() {
    let fixture = fixture().await;
    *fixture.resident.lock().await = running(None, 500);
    fixture.fail.store(true, Ordering::SeqCst);
    let (status, result) = task(&fixture).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{result}");
    let state = adaptive(&fixture).await;
    assert_eq!(state["limit"], 2);
    assert_eq!(state["last_decrease_reason"], "upstream_failure");
    assert_eq!(state["baseline_output_tokens_per_second"], json!({}));
}

#[tokio::test]
async fn host_pressure_still_throttles_without_verified_placement() {
    let fixture = fixture().await;
    *fixture.resident.lock().await = running(None, 500);
    fixture.pressure_on_completion.store(true, Ordering::SeqCst);
    let (status, result) = task(&fixture).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["status"], "unavailable");
    let state = adaptive(&fixture).await;
    assert_eq!(state["limit"], 2);
    assert_eq!(state["last_decrease_reason"], "host_memory_pressure");
    assert_eq!(state["baseline_output_tokens_per_second"], json!({}));
}
