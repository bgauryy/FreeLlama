use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;
mod common;

async fn request(platform: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = platform
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/_freellama/v1/{path}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn fixture() -> (Router, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let upstream = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[{"name":"test:latest","digest":"d","size":1}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion","vision","tools"],"model_info":{"llama.context_length":32768}})) }))
        .route("/api/chat", post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().await.push(body);
                Json(json!({"done":true,"message":{"role":"assistant","content":"ok"}}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    (
        app(&common::platform_config(
            "127.0.0.1:11435",
            endpoint,
            None,
            None,
            "test:latest",
        ))
        .unwrap(),
        captured,
        server,
    )
}

async fn create_scope(platform: &Router, messages: Value) -> String {
    let (status, result) = request(platform, "POST", "scopes", json!({"messages":messages})).await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    result["scope_id"].as_str().unwrap().to_owned()
}

fn task(id: &str) -> Value {
    json!({"scope_id":id,"scope_revision":0,"task":"completion","objective":"fastest",
        "model":"test:latest","prompt":"Read the invoice.","context_tokens":4096,
        "request_options":{"options":{"num_predict":128}}})
}

#[tokio::test]
async fn large_encoded_media_keeps_full_history_and_reports_unknown_total() {
    let (platform, captured, server) = fixture().await;
    for field in ["images", "audio"] {
        let mut original =
            json!({"role":"user","content":"Invoice attached.","thinking":"Keep this metadata."});
        original[field] = if field == "images" {
            json!(["a".repeat(20_000)])
        } else {
            json!({"data":"a".repeat(20_000)})
        };
        let id = create_scope(&platform, json!([original])).await;
        let (status, result) = request(&platform, "POST", "tasks", task(&id)).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let sizing = &result["execution"]["context_sizing"];
        assert_eq!(sizing["mode"], "scope_explicit_multimodal");
        assert!(sizing["estimated_text_input_tokens"].as_u64().unwrap() < 512);
        assert!(sizing.get("estimated_input_tokens").unwrap().is_null());
        assert!(sizing.get("modality_tokens").unwrap().is_null());
        assert_eq!(sizing["total_fit_verified"], false);
        assert_eq!(sizing["exact_token_count"], false);
        assert_eq!(sizing["tokens"], 4096);
        let calls = captured.lock().await;
        let upstream = calls.last().unwrap();
        assert_eq!(upstream["messages"][0], original);
        assert_eq!(upstream["options"]["num_ctx"], 4096);
        assert_eq!(upstream["options"]["num_predict"], 128);
        assert_eq!(upstream["truncate"], false);
        assert_eq!(upstream["shift"], false);
        drop(calls);
        let (_, history) = request(
            &platform,
            "GET",
            &format!("scopes/{id}?include_messages=true"),
            Value::Null,
        )
        .await;
        assert_eq!(history["messages"][0], original);
        assert_eq!(history["revision"], 1);
    }
    server.abort();
}

#[tokio::test]
async fn prompt_images_are_retained_and_automatic_scoped_media_is_refused() {
    let (platform, captured, server) = fixture().await;
    let id = create_scope(&platform, json!([])).await;
    let image = "a".repeat(20_000);
    let mut body = task(&id);
    body["images"] = json!([image]);
    body.as_object_mut().unwrap().remove("context_tokens");
    let (status, error) = request(&platform, "POST", "tasks", body.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(
        error.to_string().contains("explicit context_tokens"),
        "{error}"
    );
    assert!(captured.lock().await.is_empty());
    body["context_tokens"] = json!(4096);
    let (status, result) = request(&platform, "POST", "tasks", body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        captured.lock().await[0]["messages"][0]["images"],
        json!([image])
    );
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["messages"][0]["images"], json!([image]));
    server.abort();
}

#[tokio::test]
async fn textual_overflow_with_media_never_infers_or_appends() {
    let (platform, captured, server) = fixture().await;
    for field in ["content", "thinking"] {
        let mut original =
            json!({"role":"user","content":"Invoice attached.","images":["a".repeat(20_000)]});
        original[field] = json!("x".repeat(14_000));
        let id = create_scope(&platform, json!([original])).await;
        let (status, error) = request(&platform, "POST", "tasks", task(&id)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert!(
            error.to_string().contains("above configured context 4096"),
            "{error}"
        );
        let (_, history) = request(
            &platform,
            "GET",
            &format!("scopes/{id}?include_messages=true"),
            Value::Null,
        )
        .await;
        assert_eq!(history["messages"], json!([original]));
        assert_eq!(history["revision"], 0);
    }
    assert!(captured.lock().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn scoped_media_requires_positive_finite_output_and_counts_tool_schema() {
    let (platform, captured, server) = fixture().await;
    let id = create_scope(
        &platform,
        json!([{"role":"user","content":"Invoice.","images":["aW1hZ2U="]}]),
    )
    .await;
    for output in [0, -1, -2] {
        let mut body = task(&id);
        body["request_options"]["options"]["num_predict"] = json!(output);
        let (status, error) = request(&platform, "POST", "tasks", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    }
    for field in ["tools", "format"] {
        let mut body = task(&id);
        if field == "tools" {
            body[field] = json!([{"type":"function","function":{"name":"lookup","description":"x".repeat(14_000),"parameters":{"type":"object"}}}]);
        } else {
            body["request_options"][field] =
                json!({"type":"object","description":"x".repeat(14_000)});
        }
        let (status, error) = request(&platform, "POST", "tasks", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert!(
            error.to_string().contains("above configured context 4096"),
            "{error}"
        );
    }
    assert!(captured.lock().await.is_empty());
    server.abort();
}
