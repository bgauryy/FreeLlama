use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, LowMemoryMode, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

/// Low available memory alone only warns, so the OS reports pressure together with it here;
/// these contracts are about waiting for that pressure to clear.
fn governor(available: Arc<AtomicU64>) -> ResourceGovernor {
    ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            hold_available_percent: 10,
            resume_available_percent: 20,
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            recovery_samples: 2,
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        move || {
            let available = available.load(Ordering::SeqCst);
            HostResources {
                source: "injected_contract".into(),
                total_memory_bytes: Some(10_000),
                available_memory_bytes: Some(available),
                memory_pressure: Some(if available < 1000 {
                    MemoryPressure::Warning
                } else {
                    MemoryPressure::Normal
                }),
                thermal_throttled: Some(false),
                ..HostResources::default()
            }
        },
    )
    .unwrap()
}

type Execution = (Value, oneshot::Sender<()>);

async fn backend() -> (
    String,
    mpsc::UnboundedReceiver<Execution>,
    tokio::task::JoinHandle<()>,
) {
    let (send, receive) = mpsc::unbounded_channel();
    let router = Router::new()
        .route("/api/tags",get(|| async {Json(json!({"models":[{"name":"test:latest","digest":"digest-a","size":4000}]}))}))
        .route("/api/ps",get(|| async {Json(json!({"models":[]}))}))
        .route("/api/show",post(|| async {Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":32768}}))}))
        .route("/api/generate",post(|| async {Json(json!({"done":true}))}))
        .route("/api/chat",post(move |Json(body):Json<Value>| {let send=send.clone(); async move {
            let (release,released)=oneshot::channel();
            send.send((body,release)).unwrap();
            let _=released.await;
            Json(json!({"message":{"content":"ok"},"done":true}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, receive, server)
}

async fn platform(
    governor: ResourceGovernor,
    wait: Duration,
) -> (
    Router,
    mpsc::UnboundedReceiver<Execution>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let (primary, _, primary_server) = backend().await;
    let (cpu, receive, cpu_server) = backend().await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", primary, None, None)
        .with_cpu_max_queued_tasks(1)
        .with_max_queue_wait(wait);
    config.cpu_upstream = Some(cpu);
    config.cpu_models.insert("test:latest".into());
    config.resource_governor = governor;
    (
        app(&config).unwrap(),
        receive,
        vec![primary_server, cpu_server],
    )
}

fn request(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn task() -> Request<Body> {
    request(
        "/_freellama/v1/tasks",
        &json!({"task":"completion","objective":"fastest","model":"test:latest","prompt":"hello"}),
    )
}

async fn next_execution(receive: &mut mpsc::UnboundedReceiver<Execution>) -> Execution {
    tokio::time::timeout(Duration::from_secs(2), receive.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn known_pressure_holds_managed_execution_until_its_deadline_without_inference() {
    let governor = governor(Arc::new(AtomicU64::new(100)));
    let (platform, mut receive, servers) =
        platform(governor.clone(), Duration::from_millis(60)).await;
    let preview = platform
        .clone()
        .oneshot(request(
            "/_freellama/v1/routes",
            &json!({"task":"completion","objective":"fastest","model":"test:latest"}),
        ))
        .await
        .unwrap();
    assert_eq!(preview.status(), StatusCode::OK);
    let preview: Value =
        serde_json::from_slice(&to_bytes(preview.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        preview["execution"]["agent_plan"]["dispatch_readiness"],
        "held_host_pressure"
    );
    let response = tokio::time::timeout(Duration::from_secs(1), platform.clone().oneshot(task()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        receive.try_recv().is_err(),
        "pressure must prevent model execution"
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    let response = platform
        .oneshot(
            Request::get("/_freellama/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let health: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(health["backends"]["cpu"]["admission"]["slots_available"], 1);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn raw_pressure_refusal_preserves_metadata_and_unload_access() {
    let governor = governor(Arc::new(AtomicU64::new(100)));
    let (upstream, mut receive, server) = backend().await;
    let proxy = freellama::proxy::app(
        freellama::proxy::ProxyConfig::new("127.0.0.1:0", upstream, false)
            .with_resource_governor(governor.clone()),
    )
    .unwrap();
    let response = proxy
        .clone()
        .oneshot(request(
            "/api/chat",
            &json!({"model":"test:latest","messages":[]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(receive.try_recv().is_err());
    for call in [
        Request::get("/api/ps").body(Body::empty()).unwrap(),
        request("/api/show", &json!({"model":"test:latest"})),
        request(
            "/api/generate",
            &json!({"model":"test:latest","keep_alive":0}),
        ),
    ] {
        let response = proxy.clone().oneshot(call).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), usize::MAX).await.unwrap();
    }
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[tokio::test]
async fn raw_pressure_wait_does_not_hide_metadata_behind_its_inflight_cap() {
    let governor = governor(Arc::new(AtomicU64::new(100)));
    assert!(governor.snapshot().await.holding);
    let (upstream, _, server) = backend().await;
    let proxy = freellama::proxy::app(
        freellama::proxy::ProxyConfig::new("127.0.0.1:0", upstream, false)
            .with_resource_governor(governor)
            .with_max_concurrent_requests(1),
    )
    .unwrap();
    let raw_proxy = proxy.clone();
    let raw = tokio::spawn(async move {
        raw_proxy
            .oneshot(request(
                "/api/chat",
                &json!({"model":"test:latest","messages":[]}),
            ))
            .await
            .unwrap()
    });
    // Poll the spawned raw request into its governor wait before asking for recovery metadata.
    tokio::task::yield_now().await;
    let metadata = proxy
        .oneshot(Request::get("/api/ps").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    assert_eq!(raw.await.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    server.abort();
}

#[tokio::test]
async fn recovery_admits_waiting_work_and_completion_releases_forecast_memory() {
    let available = Arc::new(AtomicU64::new(100));
    let governor = governor(available.clone());
    assert!(governor.snapshot().await.holding);
    let (platform, mut receive, servers) = platform(governor.clone(), Duration::from_secs(1)).await;
    let pending = tokio::spawn(async move { platform.oneshot(task()).await.unwrap() });
    available.store(9000, Ordering::SeqCst);
    let (_, release) = next_execution(&mut receive).await;
    assert_eq!(governor.snapshot().await.reserved_bytes, 5400); // file + assumed KV + graph margin
    release.send(()).unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    for server in servers {
        server.abort();
    }
}

async fn cpu_admission(platform: &Router) -> Value {
    let response = platform
        .clone()
        .oneshot(
            Request::get("/_freellama/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    body["backends"]["cpu"]["admission"].clone()
}

#[tokio::test]
async fn memory_waiters_stay_counted_bounded_and_cancellable_after_releasing_slots() {
    let available = Arc::new(AtomicU64::new(100));
    let governor = governor(available.clone());
    let (platform, mut receive, servers) = platform(governor.clone(), Duration::from_secs(6)).await;
    let first = platform.clone();
    let pending = tokio::spawn(async move { first.oneshot(task()).await.unwrap() });
    // The execution slot is returned after two seconds of resource waiting.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(
        !pending.is_finished(),
        "resource pressure must wait within the configured deadline"
    );
    let admission = cpu_admission(&platform).await;
    assert_eq!(admission["in_flight"], 0);
    assert_eq!(admission["queue_depth"], 1, "{admission}");
    assert_eq!(admission["resource_waiters"], 1);
    assert!(
        receive.try_recv().is_err(),
        "no inference while memory is held"
    );
    let excess = tokio::time::timeout(Duration::from_millis(500), platform.clone().oneshot(task()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(excess.status(), StatusCode::TOO_MANY_REQUESTS);
    pending.abort();
    let _ = pending.await;
    assert_eq!(cpu_admission(&platform).await["queue_depth"], 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    available.store(9000, Ordering::SeqCst);
    let next = platform.clone();
    let recovered = tokio::spawn(async move { next.oneshot(task()).await.unwrap() });
    let (_, release) = next_execution(&mut receive).await;
    release.send(()).unwrap();
    assert_eq!(recovered.await.unwrap().status(), StatusCode::OK);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn a_task_waiting_without_a_slot_resumes_when_memory_recovers() {
    let available = Arc::new(AtomicU64::new(100));
    let governor = governor(available.clone());
    let (platform, mut receive, servers) = platform(governor, Duration::from_secs(6)).await;
    let first = platform.clone();
    let pending = tokio::spawn(async move { first.oneshot(task()).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(!pending.is_finished());
    assert_eq!(cpu_admission(&platform).await["in_flight"], 0);
    available.store(9000, Ordering::SeqCst);
    let (_, release) = next_execution(&mut receive).await;
    release.send(()).unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["admission"]["resource_wait_ms"].as_u64().unwrap() >= 2000);
    assert_eq!(cpu_admission(&platform).await["queue_depth"], 0);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn separate_backend_clones_share_forecast_accounting_and_drop_releases_it() {
    let governor = governor(Arc::new(AtomicU64::new(9000)));
    let first = governor
        .wait_for_capacity("http://127.0.0.1:11434", 5000, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(first.receipt.status, "admitted");
    let other_backend = governor.clone();
    let second = other_backend
        .wait_for_capacity("http://127.0.0.1:11436", 5000, Duration::ZERO)
        .await
        .expect("exhausted headroom warns instead of refusing");
    assert_eq!(
        second.receipt.status, "admitted_low_memory_estimate",
        "the second backend sees the first backend's reservation"
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 10_000);
    drop(first);
    assert_eq!(governor.snapshot().await.reserved_bytes, 5000);
    drop(second);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
}

#[tokio::test]
async fn zero_wait_still_assesses_one_slow_host_sample() {
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        || {
            // Stands in for a blocking pool that lags under load.
            std::thread::sleep(Duration::from_millis(50));
            HostResources {
                source: "slow_contract".into(),
                total_memory_bytes: Some(10_000),
                available_memory_bytes: Some(9000),
                memory_pressure: Some(MemoryPressure::Normal),
                thermal_throttled: Some(false),
                ..HostResources::default()
            }
        },
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let permit = governor
        .wait_for_capacity("http://127.0.0.1:11434", 1000, Duration::ZERO)
        .await
        .expect("a zero wait still observes the host once");
    assert!(
        permit.receipt.status.starts_with("admitted"),
        "{}",
        permit.receipt.status
    );
}

#[tokio::test]
async fn low_memory_estimate_without_os_pressure_runs_managed_work_with_a_warning() {
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        || HostResources {
            source: "advisory_contract".into(),
            total_memory_bytes: Some(10_000),
            available_memory_bytes: Some(100),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            ..HostResources::default()
        },
    )
    .unwrap();
    assert!(governor.snapshot().await.holding);
    let (platform, mut receive, servers) = platform(governor.clone(), Duration::from_secs(1)).await;
    let pending = tokio::spawn(async move { platform.oneshot(task()).await.unwrap() });
    let (_, release) = next_execution(&mut receive).await;
    release.send(()).unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let admission = &body["execution"]["resource_admission"];
    assert_eq!(
        admission["status"], "admitted_low_memory_estimate",
        "{body}"
    );
    assert_eq!(admission["assessment"]["memory_advisory"], "below_reserve");
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    for server in servers {
        server.abort();
    }
}

/// Off macOS the default refuses a load whose estimate does not fit the reserve, even without OS
/// pressure: Linux `MemAvailable` already counts reclaimable cache and is the container cap.
#[tokio::test]
async fn refuse_mode_holds_managed_work_that_does_not_fit_until_its_deadline() {
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            low_memory: LowMemoryMode::Refuse,
            ..ResourcePolicy::default()
        },
        || HostResources {
            source: "refuse_contract".into(),
            total_memory_bytes: Some(10_000),
            available_memory_bytes: Some(5000),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            ..HostResources::default()
        },
    )
    .unwrap();
    assert!(!governor.snapshot().await.holding);
    let (platform, mut receive, servers) =
        platform(governor.clone(), Duration::from_millis(60)).await;
    let response = tokio::time::timeout(Duration::from_secs(2), platform.oneshot(task()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["code"], "resource_admission_unavailable", "{body}");
    assert_eq!(body["resource_admission"]["status"], "deadline_exceeded");
    assert_eq!(
        governor
            .snapshot()
            .await
            .assess_capacity(5400, false)
            .status,
        "held_insufficient_capacity"
    );
    assert!(
        receive.try_recv().is_err(),
        "a refused load must not reach the model"
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn cancelling_active_managed_work_releases_its_forecast_reservation() {
    let governor = governor(Arc::new(AtomicU64::new(9000)));
    let (platform, mut receive, servers) = platform(governor.clone(), Duration::from_secs(1)).await;
    let pending = tokio::spawn(async move { platform.oneshot(task()).await.unwrap() });
    let (_, release) = next_execution(&mut receive).await;
    assert_eq!(governor.snapshot().await.reserved_bytes, 5400);
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    let _ = release.send(());
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn warm_forecast_is_rechecked_after_waiting_for_a_model_transition() {
    use std::sync::atomic::AtomicBool;
    let governor = governor(Arc::new(AtomicU64::new(3500)));
    let warm = Arc::new(AtomicBool::new(true));
    let ps_calls = Arc::new(AtomicU64::new(0));
    let (send, mut receive) = mpsc::unbounded_channel::<Execution>();
    let (seen_warm, seen_calls) = (warm.clone(), ps_calls.clone());
    let router=Router::new()
        .route("/api/tags",get(|| async {Json(json!({"models":[
            {"name":"blocker:latest","digest":"blocker-digest","size":0},
            {"name":"target:latest","digest":"target-digest","size":4000}]}))}))
        .route("/api/ps",get(move || {let warm=seen_warm.clone();let calls=seen_calls.clone(); async move {
            let models=if warm.load(Ordering::SeqCst) {json!([{"name":"target:latest","digest":"target-digest","size":4000,"size_vram":0,"context_length":32768}])} else {json!([])};
            calls.fetch_add(1,Ordering::SeqCst);
            Json(json!({"models":models}))
        }}))
        .route("/api/show",post(|| async {Json(json!({"capabilities":["embedding"],"model_info":{"general.architecture":"llama","llama.context_length":32768}}))}))
        .route("/api/embed",post(move |Json(body):Json<Value>| {let send=send.clone(); async move {
            let (release,released)=oneshot::channel();send.send((body,release)).unwrap();let _=released.await;
            Json(json!({"embeddings":[[0.1]]}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cpu = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (primary, _, primary_server) = backend().await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", primary, None, None)
        .with_max_queue_wait(Duration::from_secs(2))
        .with_cpu_max_concurrent_tasks(2);
    config.cpu_upstream = Some(cpu);
    config
        .cpu_models
        .extend(["blocker:latest".into(), "target:latest".into()]);
    config.resource_governor = governor.clone();
    let platform = app(&config).unwrap();
    let embedding = |model| {
        request(
            "/_freellama/v1/tasks",
            &json!({"task":"embedding","objective":"fastest","model":model,"input":"x"}),
        )
    };
    let call = embedding("blocker:latest");
    let first_platform = platform.clone();
    let blocker = tokio::spawn(async move { first_platform.oneshot(call).await.unwrap() });
    let (body, release) = next_execution(&mut receive).await;
    assert_eq!(body["model"], "blocker:latest");
    let before = ps_calls.load(Ordering::SeqCst);
    let call = embedding("target:latest");
    let target = tokio::spawn(async move { platform.oneshot(call).await.unwrap() });
    // Discovery, read-only readiness, and the initial admission footprint lookup all see the
    // resident runner. Flip residency only after the execution reservation has observed it.
    tokio::time::timeout(Duration::from_secs(1), async {
        while ps_calls.load(Ordering::SeqCst) < before + 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    warm.store(false, Ordering::SeqCst);
    release.send(()).unwrap();
    assert_eq!(blocker.await.unwrap().status(), StatusCode::OK);
    let (body, release) = next_execution(&mut receive).await;
    assert_eq!(body["model"], "target:latest");
    release.send(()).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(1), target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let admission = &body["execution"]["resource_revalidation"];
    assert!(
        admission["required_available_bytes"].as_u64().unwrap() > 0,
        "the recheck replaces the stale zero-byte forecast: {body}"
    );
    assert_eq!(
        admission["status"], "admitted_low_memory_estimate",
        "a cold runner that no longer fits the estimate dispatches with a warning"
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
    primary_server.abort();
}
