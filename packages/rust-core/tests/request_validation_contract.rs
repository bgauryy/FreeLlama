use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use freellama::platform::{PlatformConfig, app};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn invalid_task_requests_keep_structured_errors_before_execution() {
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        "http://127.0.0.1:11434",
        None,
        None,
    ))
    .unwrap();
    for (path, body, content_type, expected) in [
        ("tasks", "{", "application/json", StatusCode::BAD_REQUEST),
        (
            "tasks",
            r#"{"prompt":"x","defer":true,"timeout_seconds":-1}"#,
            "application/json",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "tasks",
            r#"{"prompt":"x","keep_alive":0}"#,
            "application/json",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "tasks",
            "{}",
            "text/plain",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "task-batches",
            "{",
            "application/json",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = platform
            .clone()
            .oneshot(
                Request::post(format!("/_freellama/v1/{path}"))
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: Value = serde_json::from_slice(&bytes)
            .expect("task validation errors must remain structured JSON");
        assert_eq!(error["code"], "invalid_task_request");
        assert!(error["error"].as_str().is_some_and(|s| !s.is_empty()));
    }
}

/// A task with nothing to work on is refused before routing, so it never loads a model, and a
/// task carrying base64 images well past axum's 2 MB default is read whole.
#[tokio::test]
async fn empty_tasks_fail_before_routing_and_large_image_tasks_are_read() {
    // Nothing listens on this upstream: a request that reached routing would fail to discover.
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        "http://127.0.0.1:9",
        None,
        None,
    ))
    .unwrap();
    let send = |body: String| {
        platform.clone().oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
    };
    for body in [
        r#"{"task":"completion","prompt":"   "}"#,
        r#"{"task":"completion"}"#,
        r#"{"task":"embedding","input":[]}"#,
        r#"{"task":"embedding","input":""}"#,
    ] {
        let response = send(body.to_owned()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            error["error"].as_str().unwrap().contains("requires"),
            "{error}"
        );
    }
    let image = "A".repeat(6 * 1024 * 1024);
    let response = send(format!(
        r#"{{"task":"vision","prompt":"read","images":["{image}"]}}"#
    ))
    .await
    .unwrap();
    // Read and validated, then refused only because the upstream is down.
    assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
