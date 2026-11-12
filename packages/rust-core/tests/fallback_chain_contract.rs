//! `fallback_models`: the next model runs only after a refusal that proves nothing executed, every
//! attempt is reported, and an error after execution started never moves to another model.

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

/// A mock Ollama with `tiny:latest` (1,024-token window), `big:latest` (32,768) and `embed:latest`
/// (embedding only). `/api/chat` fails for any model named in `failing`.
async fn backend(failing: &'static [&'static str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let chats = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&chats);
    let router = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [
                    {"name": "tiny:latest", "size": 100},
                    {"name": "big:latest", "size": 200},
                    {"name": "embed:latest", "size": 50},
                ]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|Json(body): Json<Value>| async move {
                let name = body["model"].as_str().or(body["name"].as_str()).unwrap_or("");
                let (capabilities, window) = match name {
                    "tiny:latest" => (json!(["completion"]), 1024),
                    "embed:latest" => (json!(["embedding"]), 2048),
                    _ => (json!(["completion"]), 32_768),
                };
                Json(json!({"capabilities": capabilities, "model_info": {"llama.context_length": window}}))
            }),
        )
        .route(
            "/api/chat",
            post(move |Json(body): Json<Value>| {
                let seen = Arc::clone(&seen);
                async move {
                    let model = body["model"].as_str().unwrap_or("").to_owned();
                    seen.lock().unwrap().push(model.clone());
                    if failing.contains(&model.as_str()) {
                        // A final runner error (not one the transport retries): the model ran.
                        return (StatusCode::BAD_REQUEST, Json(json!({"error": "runner failed mid-generation"})))
                            .into_response();
                    }
                    Json(json!({
                        "model": model, "done": true, "done_reason": "stop",
                        "message": {"role": "assistant", "content": "ok"},
                        "prompt_eval_count": 3, "eval_count": 1,
                    }))
                    .into_response()
                }
            }),
        )
        .route("/api/generate", post(|| async { Json(json!({"done": true})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (endpoint, chats)
}

async fn post_task(endpoint: String, task: Value) -> (StatusCode, Value) {
    let platform = app(&crate::support::platform_config(
        "127.0.0.1:11435",
        endpoint,
        None,
        None,
    ))
    .unwrap();
    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(task.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn a_model_refused_before_it_ran_moves_down_the_chain() {
    let (endpoint, chats) = backend(&[]).await;
    let (status, body) = post_task(
        endpoint,
        json!({"task": "completion", "model": "ghost:7b", "fallback_models": ["embed:latest", "big:latest"], "prompt": "hi", "keep_alive": "5m"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["fallback"]["used"], true, "{body}");
    assert_eq!(body["fallback"]["selected"], "big:latest", "{body}");
    let attempts = body["fallback"]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{body}");
    assert_eq!(attempts[0]["model"], "ghost:7b");
    assert_eq!(attempts[0]["code"], "model_not_installed");
    assert_eq!(attempts[1]["model"], "embed:latest");
    assert_eq!(attempts[1]["code"], "model_not_eligible");
    // Only the model that was admitted generated anything.
    assert_eq!(*chats.lock().unwrap(), vec!["big:latest".to_owned()]);
}

#[tokio::test]
async fn an_input_above_the_context_window_moves_to_a_larger_model() {
    let (endpoint, chats) = backend(&[]).await;
    let (status, body) = post_task(
        endpoint,
        json!({"task": "completion", "model": "tiny:latest", "fallback_models": ["big:latest"], "prompt": "word ".repeat(2000), "keep_alive": "5m"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["fallback"]["attempts"][0]["code"], "context_window_exceeded",
        "{body}"
    );
    assert_eq!(body["fallback"]["selected"], "big:latest");
    assert_eq!(*chats.lock().unwrap(), vec!["big:latest".to_owned()]);
}

#[tokio::test]
async fn a_failure_after_execution_started_is_never_replayed_on_another_model() {
    let (endpoint, chats) = backend(&["big:latest"]).await;
    let (status, body) = post_task(
        endpoint,
        json!({"task": "completion", "model": "big:latest", "fallback_models": ["tiny:latest"], "prompt": "hi", "keep_alive": "5m"}),
    )
    .await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(body["fallback"]["exhausted"], false, "{body}");
    assert_eq!(
        body["fallback"]["attempts"].as_array().unwrap().len(),
        1,
        "{body}"
    );
    assert_eq!(*chats.lock().unwrap(), vec!["big:latest".to_owned()]);
}

#[tokio::test]
async fn an_exhausted_chain_reports_every_refusal_and_a_bad_chain_is_refused_up_front() {
    let (endpoint, chats) = backend(&[]).await;
    let (status, body) = post_task(
        endpoint.clone(),
        json!({"task": "completion", "model": "ghost:1b", "fallback_models": ["ghost:2b"], "prompt": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "model_not_installed", "{body}");
    assert_eq!(body["fallback"]["exhausted"], true, "{body}");
    assert_eq!(
        body["fallback"]["attempts"].as_array().unwrap().len(),
        2,
        "{body}"
    );

    let executed = AtomicUsize::new(chats.lock().unwrap().len());
    let (status, body) = post_task(
        endpoint,
        json!({"task": "completion", "model": "big:latest", "fallback_models": ["tiny:latest", "tiny:latest"], "prompt": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("repeats"),
        "{body}"
    );
    assert_eq!(chats.lock().unwrap().len(), executed.load(Ordering::SeqCst));
}
