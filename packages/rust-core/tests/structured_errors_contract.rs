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
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;
mod common;

#[tokio::test]
async fn incomplete_or_failed_upstream_responses_never_train_feedback_and_unload_owned_requests() {
    for (status, payload) in [
        (
            StatusCode::OK,
            json!({"done":false,"message":{"content":"partial"}}),
        ),
        (StatusCode::OK, json!({"error":"runner failed"})),
        (StatusCode::BAD_REQUEST, json!({"error":"context overflow"})),
    ] {
        let inference = Arc::new(AtomicUsize::new(0));
        let unloads = Arc::new(AtomicUsize::new(0));
        let calls = inference.clone();
        let stopped = unloads.clone();
        let observed_calls = inference.clone();
        let observed_unloads = unloads.clone();
        let backend = Router::new()
            .route("/api/tags", get(|| async {Json(json!({"models":[{"name":"test:latest","size":100}]}))}))
            .route("/api/ps", get(move || {
                let calls = observed_calls.clone();
                let unloads = observed_unloads.clone();
                async move {
                    // An executed request has verifiable GPU placement until explicitly unloaded.
                    // Missing placement must not be what prevents feedback for a failed response.
                    let models = if calls.load(Ordering::SeqCst) > 0
                        && unloads.load(Ordering::SeqCst) == 0
                    {
                        json!([{"name":"test:latest","size":100,"size_vram":100}])
                    } else {
                        json!([])
                    };
                    Json(json!({"models": models}))
                }
            }))
            .route("/api/show", post(|| async {Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":32768}}))}))
            .route("/api/chat", post(move || {let calls=calls.clone(); let payload=payload.clone(); async move {
                calls.fetch_add(1,Ordering::SeqCst); (status, Json(payload))
            }}))
            .route("/api/generate", post(move || {let stopped=stopped.clone(); async move {
                stopped.fetch_add(1,Ordering::SeqCst); Json(json!({"done":true}))
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });
        let platform = app(&common::platform_config(
            "127.0.0.1:11435",
            endpoint,
            None,
            None,
            "test:latest",
        ))
        .unwrap();
        let response=platform.clone().oneshot(Request::post("/_freellama/v1/tasks")
            .header("content-type","application/json")
            .body(Body::from(json!({"task":"completion","objective":"fastest","model":"test:latest","prompt":"x","keep_alive":"0"}).to_string())).unwrap()).await.unwrap();
        assert_eq!(
            response.status(),
            if status.is_success() {
                StatusCode::BAD_GATEWAY
            } else {
                status
            }
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(body["upstream_response"].is_object());
        assert_eq!(body["lifecycle"]["status"], "verified");
        assert_eq!(
            inference.load(Ordering::SeqCst),
            1,
            "never replay incomplete execution"
        );
        assert_eq!(unloads.load(Ordering::SeqCst), 1);
        let health = platform
            .oneshot(
                Request::get("/_freellama/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let health: Value =
            serde_json::from_slice(&to_bytes(health.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(health["feedback"]["gpu"]["completed"], 0);
        assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
        server.abort();
    }
}

#[tokio::test]
async fn managed_and_batch_resource_errors_are_structured_without_executing() {
    let executed = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&executed);
    let backend = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models": [{"name": "test:latest", "size": 100}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities": ["completion"], "model_info": {"llama.context_length": 32768}})) }))
        .route("/api/chat", post(move || { let calls = Arc::clone(&calls); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(json!({"message": {"content": "unexpected execution"}}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });
    let governor = ResourceGovernor::with_sampler(ResourcePolicy::default(), || HostResources {
        source: "structured_error_fixture".into(),
        total_memory_bytes: Some(10_000),
        available_memory_bytes: Some(1),
        ..HostResources::default()
    })
    .unwrap();
    let mut config = PlatformConfig::new("127.0.0.1:11435", endpoint, None, None, "test:latest")
        .with_max_queue_wait(Duration::from_millis(30));
    config.resource_governor = governor.clone();
    let platform = app(&config).unwrap();
    let task =
        json!({"task":"completion","objective":"fastest","model":"test:latest","prompt":"hello"});
    for (path, body, batch) in [
        ("/_freellama/v1/tasks", task.clone(), false),
        (
            "/_freellama/v1/natural-routes",
            json!({"text":"answer a short question"}),
            false,
        ),
        (
            "/_freellama/v1/task-batches",
            json!({"tasks":[{"id":"a","independent":true,"task":task}]}),
            true,
        ),
    ] {
        let response = platform
            .clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if batch {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            "{path}"
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let error = if batch { &body["results"][0] } else { &body };
        assert_eq!(error["code"], "resource_admission_unavailable", "{body}");
        assert!(error["resource_admission"].is_object(), "{body}");
        let message = error["error"].as_str().unwrap();
        assert!(message.contains("resource"), "{body}");
        assert!(
            !message.starts_with('{'),
            "error must be human text: {body}"
        );
    }
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}
