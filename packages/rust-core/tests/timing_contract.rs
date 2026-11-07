//! Timing arithmetic in managed-task receipts, against a simulated Ollama whose runners take
//! different times to leave `/api/ps` after an unload request. No live model is involved.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    app,
    resources::{HostResources, LowMemoryMode, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::AtomicU64, atomic::Ordering},
    time::{Duration, Instant},
};
use tower::ServiceExt;

const GIB: u64 = 1024 * 1024 * 1024;
/// Idle runners and how long each stays listed by `/api/ps` after its unload request. Any two
/// differ by far more than the 200 ms confirmation poll, whichever victims the plan picks.
const RUNNERS: [(&str, u64); 3] = [
    ("idle-a:latest", 100),
    ("idle-b:latest", 600),
    ("idle-c:latest", 1100),
];

struct Fleet {
    loaded: Mutex<Vec<String>>,
    /// When an unloaded runner stops being reported.
    leaving: Mutex<BTreeMap<String, Instant>>,
    available: AtomicU64,
}

async fn ollama(fleet: Arc<Fleet>) -> String {
    let names: Vec<&str> = std::iter::once("target:latest")
        .chain(RUNNERS.iter().map(|(name, _)| *name))
        .collect();
    let (ps, generate, chat) = (fleet.clone(), fleet.clone(), fleet);
    let router = Router::new()
        .route(
            "/api/tags",
            get(move || {
                let models: Vec<Value> = names
                    .iter()
                    .map(|name| json!({"name":name,"model":name,"size":2 * GIB,"digest":format!("digest-{name}")}))
                    .collect();
                async move { Json(json!({ "models": models })) }
            }),
        )
        .route(
            "/api/ps",
            get(move || {
                let fleet = ps.clone();
                async move {
                    let now = Instant::now();
                    let leaving = fleet.leaving.lock().unwrap().clone();
                    let models: Vec<Value> = fleet
                        .loaded
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|name| leaving.get(*name).is_none_or(|gone| *gone > now))
                        .map(|name| json!({
                            "name":name,"model":name,"size":2 * GIB,"size_vram":0,
                            "digest":format!("digest-{name}"),"context_length":4096,
                            "expires_at":"2026-09-29T12:00:00Z",
                        }))
                        .collect();
                    Json(json!({ "models": models }))
                }
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":8192}}))
            }),
        )
        .route(
            "/api/generate",
            post(move |Json(body): Json<Value>| {
                let fleet = generate.clone();
                async move {
                    if body["keep_alive"] == 0 {
                        let name = body["model"].as_str().unwrap_or_default().to_owned();
                        let delay = RUNNERS
                            .iter()
                            .find(|(runner, _)| *runner == name)
                            .map_or(0, |(_, delay)| *delay);
                        fleet
                            .leaving
                            .lock()
                            .unwrap()
                            .insert(name, Instant::now() + Duration::from_millis(delay));
                        fleet.available.fetch_add(2 * GIB, Ordering::SeqCst);
                    }
                    Json(json!({"done":true,"response":""}))
                }
            }),
        )
        .route(
            "/api/chat",
            post(move |Json(body): Json<Value>| {
                let fleet = chat.clone();
                async move {
                    let name = body["model"].as_str().unwrap_or_default().to_owned();
                    let mut loaded = fleet.loaded.lock().unwrap();
                    if !loaded.contains(&name) {
                        loaded.push(name);
                    }
                    Json(json!({
                        "message":{"role":"assistant","content":"ok"},"done":true,
                        "prompt_eval_count":3,"eval_count":2,"load_duration":1_000_000_000_u64,
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

/// One task that needs memory held by idle runners, then the status view after it.
async fn load_that_evicts() -> (Value, Value) {
    let fleet = Arc::new(Fleet {
        loaded: Mutex::new(RUNNERS.iter().map(|(name, _)| (*name).to_owned()).collect()),
        leaving: Mutex::default(),
        // Less than the target needs: at least two 2 GiB runners must leave.
        available: AtomicU64::new(GIB),
    });
    let cpu = ollama(fleet.clone()).await;
    let gpu = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/api/tags", get(|| async { Json(json!({"models":[]})) })),
            )
            .await
            .unwrap();
        });
        format!("http://{address}")
    };
    let memory = fleet.clone();
    let mut config = crate::support::platform_config("127.0.0.1:11435", &gpu, None, None)
        .with_cpu_backend(
            &cpu,
            std::iter::once("target:latest").chain(RUNNERS.iter().map(|(name, _)| *name)),
        );
    config.resource_governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "fixture".into(),
            total_memory_bytes: Some(32 * GIB),
            available_memory_bytes: Some(memory.available.load(Ordering::SeqCst)),
            ..HostResources::default()
        },
    )
    .unwrap();
    let platform = app(&config).unwrap();
    let response = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"task":"completion","model":"target:latest","prompt":"hi","max_wait_seconds":30}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let receipt = json_body(response).await;
    let status = json_body(
        platform
            .oneshot(
                Request::get("/_freellama/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    (receipt, status)
}

#[tokio::test]
async fn total_wait_is_the_sum_of_disjoint_queue_transition_and_resource_waits() {
    let (receipt, _) = load_that_evicts().await;
    let eviction = &receipt["execution"]["memory_reservation"]["eviction"];
    assert!(
        eviction.as_array().is_some_and(|rounds| !rounds.is_empty()),
        "the scenario must evict, otherwise it proves nothing: {receipt}"
    );
    let admission = &receipt["admission"];
    let wait = |key: &str| {
        admission[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key}: {admission}"))
    };
    let (queue, transition, resource) = (
        wait("queue_wait_ms"),
        wait("transition_wait_ms"),
        wait("resource_wait_ms"),
    );
    assert_eq!(
        wait("total_wait_ms"),
        queue + transition + resource,
        "waits must be disjoint: {admission}"
    );
    // Evicting the first runner alone takes at least its 100 ms listing delay; that time is a
    // wait the caller experienced, so it must appear in exactly one of the parts.
    assert!(
        wait("total_wait_ms") >= 100,
        "readmission after eviction hid its wait: {admission}"
    );
}

#[tokio::test]
async fn each_victim_reports_its_own_unload_duration_not_the_batch_duration() {
    let (receipt, status) = load_that_evicts().await;
    let events = status["residency"]["audit"]["recent"].as_array().unwrap();
    let durations: BTreeMap<&str, u64> = events
        .iter()
        .filter(|event| event["event"] == "evict")
        .map(|event| {
            (
                event["model"].as_str().unwrap(),
                event["duration_ms"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(
        durations.len() >= 2,
        "two victims are needed to compare their durations: {events:?}"
    );
    for (model, duration) in &durations {
        let delay = RUNNERS.iter().find(|(name, _)| name == model).unwrap().1;
        assert!(
            *duration >= delay,
            "{model} was listed for {delay} ms after its request but reports {duration} ms"
        );
    }
    let (fastest, slowest) = (
        durations.values().min().unwrap(),
        durations.values().max().unwrap(),
    );
    assert!(
        slowest - fastest >= 300,
        "per-victim durations must differ like the runners did: {durations:?}"
    );
    let batch = receipt["execution"]["memory_reservation"]["eviction"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|round| round["unload_ms"].as_u64())
        .max()
        .expect("the eviction receipt reports the batch duration");
    assert!(
        batch >= *slowest,
        "the batch covers its slowest victim: {batch} < {slowest}"
    );
    assert!(
        fastest < &batch,
        "the fastest victim must not be charged the batch duration: {durations:?} vs {batch}"
    );
}
