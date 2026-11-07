//! Speculative warm contracts against a mock Ollama; no local model processes are touched.
use super::*;
use crate::platform::{
    PlatformConfig,
    resources::{HostResources, LowMemoryMode, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use axum::{
    Router,
    routing::{get, post},
};
use serde_json::json;
use std::sync::atomic::AtomicU64;
use tokio::sync::Mutex as AsyncMutex;

const MODEL: &str = "predicted:latest";
const GIB: u64 = 1 << 30;

#[derive(Clone, Default)]
struct Engine {
    loaded: Arc<AsyncMutex<Vec<Value>>>,
    calls: Arc<AsyncMutex<Vec<Value>>>,
}

struct Fixture {
    state: PlatformState,
    engine: Engine,
    available: Arc<AtomicU64>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture() -> Fixture {
    let engine = Engine::default();
    let router = Router::new()
        .route("/api/tags", get(|| async {
            Json(json!({"models": [{"name": MODEL, "digest": "prediction-fixture", "size": GIB}]}))
        }))
        .route("/api/show", post(|| async {
            Json(json!({"capabilities": ["completion"], "model_info": {
                "general.architecture": "llama", "llama.context_length": 8192
            }}))
        }))
        .route("/api/ps", get(|State(engine): State<Engine>| async move {
            Json(json!({"models": engine.loaded.lock().await.clone()}))
        }))
        .route("/api/chat", post(|State(engine): State<Engine>, Json(body): Json<Value>| async move {
            engine.calls.lock().await.push(body);
            Json(json!({"message": {"role": "assistant", "content": ""}, "done": true,
                "done_reason": "load", "load_duration": 300_000_000_u64}))
        }))
        .route("/api/generate", post(|State(engine): State<Engine>, Json(body): Json<Value>| async move {
            engine.calls.lock().await.push(body);
            Json(json!({"done": true}))
        }))
        .with_state(engine.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let available = Arc::new(AtomicU64::new(64 * GIB));
    let observed = Arc::clone(&available);
    let mut config = PlatformConfig::new("127.0.0.1:0", upstream, None, None)
        .with_max_concurrent_tasks(1)
        .with_max_queue_wait(Duration::from_secs(30));
    config.resource_governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "predictive_contract".into(),
            total_memory_bytes: Some(128 * GIB),
            available_memory_bytes: Some(observed.load(Ordering::Acquire)),
            memory_pressure: Some(MemoryPressure::Normal),
            gpu_memory_total_bytes: Some(64 * GIB),
            gpu_memory_free_bytes: Some(observed.load(Ordering::Acquire)),
            gpu_telemetry_source: Some("injected".into()),
            ..HostResources::default()
        },
    )
    .unwrap();
    let (_, state) = crate::platform::build(&config).unwrap();
    Fixture {
        state,
        engine,
        available,
        server,
    }
}

async fn predict(fixture: &Fixture) -> Result<u64, ApiError> {
    let input = warming::predicted_task(MODEL.into()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), run_warm(&fixture.state, input))
        .await
        .expect("speculation must not wait for the 30-second foreground queue deadline")
}

#[tokio::test]
async fn predicted_warm_uses_the_existing_ollama_load_request() {
    let fixture = fixture().await;
    predict(&fixture).await.unwrap();
    let calls = fixture.engine.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], MODEL);
    assert_eq!(calls[0]["messages"], json!([]));
    assert!(calls[0].get("opportunistic_warm").is_none());
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 0);
}

#[tokio::test]
async fn prediction_does_not_queue_when_foreground_claims_capacity_after_precheck() {
    let fixture = fixture().await;
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 0);
    let (_foreground, _) = fixture
        .state
        .gpu_admission
        .acquire(
            1,
            TaskPriority::Interactive,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_or_else(|_| panic!("foreground admission should be available"));
    let error = predict(&fixture).await.unwrap_err();
    assert!(error.is_opportunistic_warm_skip());
    assert_eq!(fixture.state.gpu_admission.receipt()["queue_depth"], 0);
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 1);
    assert!(fixture.engine.calls.lock().await.is_empty());
}

#[tokio::test]
async fn prediction_does_not_take_capacity_from_resource_waiters() {
    let fixture = fixture().await;
    let _waiting = fixture
        .state
        .gpu_admission
        .register_resource_waiter()
        .unwrap_or_else(|_| panic!("resource waiter registration should be available"));
    let before = fixture.state.gpu_admission.receipt()["queue_depth"].clone();
    let error = predict(&fixture).await.unwrap_err();
    assert!(error.is_opportunistic_warm_skip());
    assert_eq!(fixture.state.gpu_admission.receipt()["queue_depth"], before);
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 0);
    assert!(fixture.engine.calls.lock().await.is_empty());
}

#[tokio::test]
async fn prediction_releases_capacity_when_another_transition_is_active() {
    let fixture = fixture().await;
    let _transition = Arc::clone(&fixture.state.load_lock).lock_owned().await;
    let error = predict(&fixture).await.unwrap_err();
    assert!(error.is_opportunistic_warm_skip());
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 0);
    assert_eq!(fixture.state.gpu_admission.receipt()["queue_depth"], 0);
    assert!(fixture.engine.calls.lock().await.is_empty());
}

#[tokio::test]
async fn prediction_rechecks_fallen_memory_even_when_normal_admission_is_advisory() {
    let fixture = fixture().await;
    let target = execution::primary_target(&fixture.state);
    let model = catalog::primary_model(&fixture.state, MODEL).await.unwrap();
    assert!(!crate::platform::raw_models::may_need_room(&fixture.state, &target, &model).await);
    fixture.available.store(0, Ordering::Release);
    fixture.state.resources.invalidate().await;
    let error = predict(&fixture).await.unwrap_err();
    assert!(error.is_opportunistic_warm_skip());
    assert_eq!(fixture.state.gpu_admission.receipt()["in_flight"], 0);
    assert!(
        fixture.engine.calls.lock().await.is_empty(),
        "neither load nor unload is allowed"
    );
}

#[tokio::test]
async fn prediction_keeps_other_models_when_engine_resident_limit_is_unknown() {
    let fixture = fixture().await;
    let resident = json!({"name": "useful:latest", "size": GIB, "size_vram": GIB,
        "expires_at": "2099-01-01T00:00:00Z"});
    fixture.engine.loaded.lock().await.push(resident.clone());
    let error = predict(&fixture).await.unwrap_err();
    assert!(error.is_opportunistic_warm_skip());
    assert_eq!(*fixture.engine.loaded.lock().await, vec![resident]);
    assert!(
        fixture.engine.calls.lock().await.is_empty(),
        "must not ask the engine to displace its runner"
    );
}
