use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{app, runtime_metrics};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

#[tokio::test]
async fn oversized_intent_is_refused_before_contacting_the_backend() {
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        "http://127.0.0.1:1",
        None,
        None,
        "test",
    ))
    .unwrap();
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/natural-routes")
                .header("content-type", "application/json")
                .body(Body::from(json!({"text":"語".repeat(4000)}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("interpreter context budget"));
}

#[tokio::test]
async fn raw_inference_cannot_overlap_managed_work_but_metadata_remains_available() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let (started_handler, finish_handler) = (Arc::clone(&started), Arc::clone(&finish));
    let upstream = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[{"name":"test:latest","size":1}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":32768}})) }))
        .route("/api/chat", post(move || {let started=Arc::clone(&started_handler); let finish=Arc::clone(&finish_handler); async move {
            started.notify_one();
            finish.notified().await;
            Json(json!({"message":{"content":"ok"},"done":true}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        format!("http://{address}"),
        None,
        None,
        "test:latest",
    ))
    .unwrap();
    let managed_platform = platform.clone();
    let managed = tokio::spawn(async move {
        managed_platform
            .oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"task":"completion","objective":"fastest","prompt":"hello"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let raw = platform
        .clone()
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(raw.status(), StatusCode::SERVICE_UNAVAILABLE);
    let metadata = platform
        .oneshot(Request::get("/api/ps").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    finish.notify_one();
    assert_eq!(managed.await.unwrap().status(), StatusCode::OK);
    server.abort();
}

#[test]
fn prefill_metrics_exclude_cached_tokens_and_preserve_unknowns() {
    let metrics = runtime_metrics(
        &json!({"prompt_eval_count":10,"prompt_eval_cached_count":4,"prompt_eval_duration":1_000_000_000}),
    );
    assert_eq!(metrics["prompt_tokens"], 10);
    assert_eq!(metrics["cached_prompt_tokens"], 4);
    assert_eq!(metrics["uncached_prompt_tokens"], 6);
    assert_eq!(metrics["prompt_tokens_per_second"], 6.0);
    let unknown =
        runtime_metrics(&json!({"prompt_eval_count":10,"prompt_eval_duration":1_000_000_000}));
    assert!(unknown["cached_prompt_tokens"].is_null());
    assert!(unknown["prompt_tokens_per_second"].is_null());
    let invalid = runtime_metrics(
        &json!({"prompt_eval_count":10,"prompt_eval_cached_count":11,"prompt_eval_duration":1_000_000_000}),
    );
    assert!(invalid["uncached_prompt_tokens"].is_null());
}

#[tokio::test]
async fn managed_text_sizes_context_and_refuses_oversize_before_inference() {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = Arc::clone(&captured);
    let upstream = Router::new()
        .route("/api/tags",get(|| async {Json(json!({"models":[{"name":"test:latest","size":1}]}))}))
        .route("/api/ps",get(|| async {Json(json!({"models":[]}))}))
        .route("/api/show",post(|| async {Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":32768}}))}))
        .route("/api/chat",post(move |Json(body):Json<Value>| {let seen=Arc::clone(&seen); async move {
            seen.lock().await.push(body);
            Json(json!({"message":{"role":"assistant","content":"ok"},"done":true}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        format!("http://{address}"),
        None,
        None,
        "test:latest",
    ))
    .unwrap();
    for (prompt, output, status) in [
        ("hello".to_owned(), 512, StatusCode::OK),
        ("hello".to_owned(), 3000, StatusCode::OK),
        ("x".repeat(100_000), 512, StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let response = platform
            .clone()
            .oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"task":"completion","objective":"fastest","prompt":prompt,
                            "request_options":{"options":{"num_predict":output}}})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let actual = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
        if actual == StatusCode::OK {
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            // num_thread is part of Ollama's runner identity; injecting it on the shared GPU
            // backend made raw clients and managed tasks reload each other's runner.
            assert!(body["execution"]["runtime_options"]["num_thread"].is_null());
            assert_eq!(
                body["execution"]["runtime_options"],
                captured.lock().await.last().unwrap()["options"]
            );
            assert_eq!(
                body["execution"]["context_sizing"]["mode"],
                "prompt_estimate"
            );
            assert!(body["admission"]["transition_wait_ms"].is_number());
            assert_eq!(
                body["route"]["options"]["num_ctx"],
                body["execution"]["context_sizing"]["tokens"]
            );
            assert_eq!(
                body["route"]["options"]["num_ctx"],
                captured.lock().await.last().unwrap()["options"]["num_ctx"]
            );
        }
    }
    let calls = captured.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["options"]["num_ctx"], 2048);
    assert_eq!(calls[0]["truncate"], false);
    assert_eq!(calls[0]["shift"], false);
    assert_eq!(calls[1]["options"]["num_ctx"], 4096);
    drop(calls);
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"task":"completion","objective":"fastest","prompt":"hello",
                "request_options":{"options":{"num_thread":1}}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        captured.lock().await.last().unwrap()["options"]["num_thread"],
        1
    );
    server.abort();
}
#[tokio::test]
async fn managed_requests_preserve_caller_messages_without_injecting_system_instructions() {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = Arc::clone(&captured);
    let upstream = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[{"name":"test:latest","size":1}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":32768}})) }))
        .route("/api/chat", post(move |Json(body): Json<Value>| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().await.push(body);
                Json(json!({"message":{"content":"ok"},"done":true}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        endpoint,
        None,
        None,
        "test:latest",
    ))
    .unwrap();
    for messages in [
        json!([{"role":"system","content":"  My system prompt.\nענה בעברית.\n"}, {"role":"user","content":"hello"}]),
        json!([{"role":"system","content":"First"}, {"role":"system","content":"Second"}, {"role":"assistant","content":"Earlier","thinking":"caller metadata"}, {"role":"user","content":"Next"}]),
        json!([{"role":"user","content":"No system message"}]),
    ] {
        let response = platform.clone().oneshot(Request::post("/_freellama/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(json!({"task":"coding","objective":"fastest","messages":messages,"prompt":"ignored because messages win"}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(captured.lock().await.last().unwrap()["messages"], messages);
    }
    let prompt = "  Unwrapped caller prompt\n";
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(json!({"prompt":prompt}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let receipt: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(receipt["route"]["task"], "completion");
    assert_eq!(receipt["route"]["confidence"], "low");
    assert_eq!(receipt["route"]["quality_evidence"], "none");
    assert_eq!(
        captured.lock().await.last().unwrap()["messages"],
        json!([{"role":"user","content":prompt}])
    );
    server.abort();
}

mod common;
