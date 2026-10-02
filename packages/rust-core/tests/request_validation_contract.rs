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
        "helper:latest",
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
