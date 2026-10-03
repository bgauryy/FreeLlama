//! Control-plane latency measurements use simulated runners; they do not measure inference.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::Request,
    routing::{get, post},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tower::ServiceExt;

async fn backend(
    delay: Duration,
    started: Arc<AtomicUsize>,
    wait_for_peer: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new().route(
        "/api/ps",
        get(move || {
            let started = started.clone();
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                if wait_for_peer {
                    tokio::time::timeout(Duration::from_millis(250), async {
                        while started.load(Ordering::SeqCst) < 2 {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("status must start both backend reads without waiting for the first");
                }
                tokio::time::sleep(delay).await;
                Json(json!({"models":[]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}

async fn measure(delays: [Duration; 2], peers: bool, trials: usize) -> Vec<f64> {
    let started = Arc::new(AtomicUsize::new(0));
    let (gpu, gpu_server) = backend(delays[0], started.clone(), peers).await;
    let (cpu, cpu_server) = backend(delays[1], started, peers).await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", gpu, None, None, "helper:latest");
    config.cpu_upstream = Some(cpu);
    config.cpu_models.insert("cpu-helper:latest".into());
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), HostResources::default).unwrap();
    let platform = app(&config).unwrap();
    let mut times = Vec::new();
    for _ in 0..trials {
        let start = Instant::now();
        let response = platform
            .clone()
            .oneshot(
                Request::get("/_freellama/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(
            body["loaded_models"].as_array().unwrap().is_empty(),
            "{body}"
        );
        assert!(body["backends"]["gpu"].is_object() && body["backends"]["cpu"].is_object());
    }
    gpu_server.abort();
    cpu_server.abort();
    times
}

#[tokio::test]
async fn status_queries_independent_backends_concurrently() {
    measure([Duration::ZERO; 2], true, 1).await;
}

#[tokio::test]
#[ignore = "fixed-budget timing experiment; run separately to avoid test-suite CPU noise"]
async fn measure_status_latency() {
    for (name, delays) in [("balanced", [120, 120]), ("held_out_asymmetric", [40, 180])] {
        let mut samples = measure(delays.map(Duration::from_millis), false, 20).await;
        samples.sort_by(f64::total_cmp);
        println!(
            "CONTROL_TIMING {}",
            json!({"case":name,"trials":samples.len(),"p50_ms":samples[9],"p95_ms":samples[18],"samples_ms":samples})
        );
    }
}

async fn catalog_backend(
    delay: Duration,
    name: &'static str,
    peers: Option<Arc<AtomicUsize>>,
) -> (String, tokio::task::JoinHandle<()>) {
    async fn inventory(
        delay: Duration,
        peers: Option<Arc<AtomicUsize>>,
        value: Value,
    ) -> Json<Value> {
        if let Some(peers) = peers {
            peers.fetch_add(1, Ordering::SeqCst);
            tokio::time::timeout(Duration::from_millis(250), async {
                while peers.load(Ordering::SeqCst) < 4 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cold discovery must start independent backend inventory reads concurrently");
        }
        tokio::time::sleep(delay).await;
        Json(value)
    }
    let tags_peers = peers.clone();
    let router = Router::new()
        .route("/api/tags", get(move || inventory(delay, tags_peers.clone(), json!({"models":[{"name":name,"size":1000,"digest":name}]}))))
        .route("/api/ps", get(move || inventory(delay, peers.clone(), json!({"models":[]}))))
        .route("/api/show", post(move || async move { tokio::time::sleep(delay).await; Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":8192}})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}

async fn measure_discovery(delay: Duration, peers: bool, trials: usize) -> (Vec<f64>, Vec<f64>) {
    let counter = peers.then(|| Arc::new(AtomicUsize::new(0)));
    let (gpu, gpu_server) = catalog_backend(delay, "helper:latest", counter.clone()).await;
    let (cpu, cpu_server) = catalog_backend(delay, "cpu-helper:latest", counter).await;
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for _ in 0..trials {
        let mut config = PlatformConfig::new("127.0.0.1:11435", &gpu, None, None, "helper:latest");
        config.cpu_upstream = Some(cpu.clone());
        config.cpu_models.insert("cpu-helper:latest".into());
        config.resource_governor =
            ResourceGovernor::with_sampler(ResourcePolicy::default(), HostResources::default)
                .unwrap();
        let platform = app(&config).unwrap();
        for samples in [&mut cold, &mut warm] {
            let start = Instant::now();
            let response = platform
                .clone()
                .oneshot(
                    Request::get("/_freellama/v1/models")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            let names = body["models"]
                .as_array()
                .expect("both catalogs must survive discovery")
                .iter()
                .map(|m| m["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(names, ["cpu-helper:latest", "helper:latest"]);
        }
    }
    gpu_server.abort();
    cpu_server.abort();
    (cold, warm)
}

#[tokio::test]
async fn cold_discovery_queries_independent_inventories_concurrently() {
    measure_discovery(Duration::ZERO, true, 1).await;
}

#[tokio::test]
#[ignore = "fixed-budget discovery experiment; simulated upstream delays"]
async fn measure_discovery_latency() {
    let (cold, warm) = measure_discovery(Duration::from_millis(80), false, 20).await;
    for (name, mut samples) in [("discovery_cold", cold), ("discovery_warm", warm)] {
        samples.sort_by(f64::total_cmp);
        println!(
            "CONTROL_TIMING {}",
            json!({"case":name,"trials":samples.len(),"p50_ms":samples[9],"p95_ms":samples[18],"samples_ms":samples})
        );
    }
}
