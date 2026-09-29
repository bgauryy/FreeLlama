use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;

async fn fixture(
    available: Option<u64>,
    resident: bool,
) -> (
    Router,
    ResourceGovernor,
    String,
    tokio::task::JoinHandle<()>,
) {
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            hold_available_percent: 10,
            resume_available_percent: 20,
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "readiness_fixture".into(),
            total_memory_bytes: Some(10_000),
            available_memory_bytes: available,
            ..HostResources::default()
        },
    )
    .unwrap();
    let upstream = Router::new()
        .route("/api/tags", get(|| async {Json(json!({"models":[{"name":"test:latest","digest":"digest-a","size":4000}]}))}))
        .route("/api/ps", get(move || async move {Json(json!({"models": if resident {
            vec![json!({"name":"test:latest","digest":"digest-a","context_length":8192,"size":4000,"size_vram":0})]
        } else {vec![]}}))}))
        .route("/api/show", post(|| async {Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":32768}}))}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let primary_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_endpoint = format!("http://{}", primary_listener.local_addr().unwrap());
    let primary = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }));
    let server = tokio::spawn(async move {
        let (a, b) = tokio::join!(
            axum::serve(listener, upstream).into_future(),
            axum::serve(primary_listener, primary).into_future()
        );
        a.unwrap();
        b.unwrap();
    });
    // Explicit CPU assignment makes the host-memory contract portable to non-unified CI machines.
    let mut config = PlatformConfig::new(
        "127.0.0.1:11435",
        primary_endpoint,
        None,
        None,
        "test:latest",
    )
    .with_cpu_backend(endpoint.clone(), vec!["test:latest".to_owned()]);
    config.resource_governor = governor.clone();
    (app(&config).unwrap(), governor, endpoint, server)
}

async fn preview(platform: Router) -> Value {
    let response = platform.oneshot(Request::post("/_freellama/v1/routes")
        .header("content-type", "application/json")
        .body(Body::from(json!({"task":"completion","objective":"fastest","model":"test:latest","context_tokens":8192}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn preview_does_not_call_a_cold_model_runnable_when_its_memory_wont_fit() {
    let (platform, governor, _, server) = fixture(Some(3500), false).await;
    let result = preview(platform).await;
    assert_eq!(
        result["execution"]["agent_plan"]["queue_readiness"],
        "runnable_now"
    );
    assert_eq!(
        result["execution"]["agent_plan"]["dispatch_readiness"],
        "held_insufficient_capacity"
    );
    assert_eq!(
        result["execution"]["agent_plan"]["independent_tasks_admissible_now"],
        0
    );
    assert_eq!(
        result["execution"]["resource_assessment"]["required_available_bytes"],
        // 4000-byte file with unknown KV shape: + 25% assumed KV + 10% graph margin.
        5400
    );
    assert_eq!(
        governor.snapshot().await.reserved_bytes,
        0,
        "preview must not reserve memory"
    );
    server.abort();
}

#[tokio::test]
async fn preview_reports_unknown_required_memory_instead_of_runnable() {
    let (platform, governor, _, server) = fixture(None, false).await;
    let result = preview(platform).await;
    assert_eq!(
        result["execution"]["agent_plan"]["dispatch_readiness"],
        "held_telemetry_unavailable"
    );
    assert_eq!(
        result["execution"]["resource_assessment"]["admissible"],
        false
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[tokio::test]
async fn fitting_preview_reports_model_footprint_without_acquiring_it() {
    let (platform, governor, _, server) = fixture(Some(9000), false).await;
    let result = preview(platform).await;
    assert_eq!(
        result["execution"]["agent_plan"]["dispatch_readiness"],
        "runnable_now"
    );
    assert_eq!(
        result["execution"]["resource_assessment"]["admissible"],
        true
    );
    assert_eq!(
        result["execution"]["memory_reservation"]["required_available_bytes"],
        5400
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[tokio::test]
async fn preview_subtracts_existing_cross_backend_reservations() {
    let (platform, governor, endpoint, server) = fixture(Some(9000), false).await;
    let held = governor
        .wait_for_capacity(&endpoint, 5000, Duration::from_secs(1))
        .await
        .unwrap();
    let result = preview(platform).await;
    assert_eq!(
        result["execution"]["agent_plan"]["dispatch_readiness"],
        "held_insufficient_capacity"
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 5000);
    drop(held);
    server.abort();
}

#[tokio::test]
async fn verified_resident_context_does_not_require_a_second_copy_of_model_weights() {
    let (platform, governor, _, server) = fixture(Some(3500), true).await;
    let result = preview(platform).await;
    assert_eq!(
        result["execution"]["agent_plan"]["dispatch_readiness"],
        "runnable_now"
    );
    assert_eq!(
        result["execution"]["memory_reservation"]["source"],
        "matching_resident_context"
    );
    assert_eq!(
        result["execution"]["resource_assessment"]["required_available_bytes"],
        0
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}
