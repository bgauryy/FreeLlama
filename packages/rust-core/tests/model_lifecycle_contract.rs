//! Load, unload and readiness as a caller sees them: an unload names a model, not a task; a
//! warm without a task loads a model as what it is; and `/ready` answers whether work can be served, not just whether the port is open.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(platform: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = platform
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A one-model Ollama: `embed:1` is embedding-only, `/api/embed` loads it, `keep_alive: 0` on
/// `/api/generate` unloads it, and `/api/chat` must never be called. Every `/api/embed` body is
/// captured.
struct Ollama {
    resident: Arc<AtomicBool>,
    embeds: Arc<Mutex<Vec<Value>>>,
    unloads: Arc<Mutex<u32>>,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

async fn embedding_ollama(resident: bool) -> Ollama {
    let state = Arc::new(AtomicBool::new(resident));
    let embeds = Arc::new(Mutex::new(Vec::new()));
    let unloads = Arc::new(Mutex::new(0));
    let (ps, load, unload) = (state.clone(), state.clone(), state.clone());
    let (captured, unload_count) = (embeds.clone(), unloads.clone());
    let upstream = Router::new()
        .route("/api/version", get(|| async { Json(json!({"version":"0.40.13"})) }))
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models":[{"name":"embed:1","digest":"e","size":274_000_000}]}))
            }),
        )
        .route(
            "/api/ps",
            get(move || {
                let ps = ps.clone();
                async move {
                    Json(json!({"models": if ps.load(Ordering::SeqCst) {
                        vec![json!({"name":"embed:1","digest":"e","size":335_000_000,"size_vram":335_000_000,"context_length":2048,"expires_at":"2099-01-01T00:00:00Z"})]
                    } else { vec![] }}))
                }
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({"capabilities":["embedding"],"model_info":{"nomic-bert.context_length":2048}}))
            }),
        )
        .route(
            "/api/embed",
            post(move |Json(body): Json<Value>| {
                let (load, captured) = (load.clone(), captured.clone());
                async move {
                    captured.lock().unwrap().push(body);
                    load.store(true, Ordering::SeqCst);
                    Json(json!({"model":"embed:1","embeddings":[[0.1,0.2]],"total_duration":1_000_000,"load_duration":500_000,"prompt_eval_count":2}))
                }
            }),
        )
        .route(
            "/api/generate",
            post(move |Json(body): Json<Value>| {
                let (unload, count) = (unload.clone(), unload_count.clone());
                async move {
                    assert_eq!(body["keep_alive"], 0, "{body}");
                    *count.lock().unwrap() += 1;
                    unload.store(false, Ordering::SeqCst);
                    Json(json!({"done":true,"done_reason":"unload"}))
                }
            }),
        )
        .route(
            "/api/chat",
            // An embedding-only model must never be sent to chat; failing here fails the caller.
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":"embedding-only model sent to /api/chat"})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    Ollama {
        resident: state,
        embeds,
        unloads,
        endpoint,
        server,
    }
}

fn platform(endpoint: &str) -> Router {
    app(&crate::support::platform_config(
        "127.0.0.1:11435",
        endpoint.to_owned(),
        None,
        None,
    ))
    .unwrap()
}

#[tokio::test]
async fn unloading_a_resident_model_needs_no_task() {
    let ollama = embedding_ollama(true).await;
    let platform = platform(&ollama.endpoint);
    let (status, receipt) = send(
        &platform,
        "POST",
        "/_freellama/v1/warm",
        json!({"model":"embed:1","keep_alive":"0"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["unloaded"], true, "{receipt}");
    assert_eq!(receipt["status"], "unloaded");
    assert_eq!(receipt["backend"], "gpu");
    assert!(!ollama.resident.load(Ordering::SeqCst));
    assert_eq!(*ollama.unloads.lock().unwrap(), 1);
    assert!(
        ollama.embeds.lock().unwrap().is_empty(),
        "an unload must not load or run the model"
    );
    ollama.server.abort();
}

#[tokio::test]
async fn warming_an_embedding_model_without_a_task_loads_it_as_embedding() {
    let ollama = embedding_ollama(false).await;
    let platform = platform(&ollama.endpoint);
    let (status, receipt) = send(
        &platform,
        "POST",
        "/_freellama/v1/warm",
        json!({"model":"embed:1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert!(ollama.resident.load(Ordering::SeqCst));
    assert_eq!(ollama.embeds.lock().unwrap().len(), 1);
    ollama.server.abort();
}

#[tokio::test]
async fn ready_reports_what_is_missing() {
    let ollama = embedding_ollama(false).await;
    let (status, ready) = send(
        &platform(&ollama.endpoint),
        "GET",
        "/_freellama/v1/ready",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["ollama"]["version"], "0.40.13");
    assert_eq!(ready["models"]["installed"], 1);
    ollama.server.abort();

    // Nothing listens here: liveness still answers, readiness names Ollama.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let platform = platform(&closed);
    let (status, ready) = send(&platform, "GET", "/_freellama/v1/ready", Value::Null).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{ready}");
    assert_eq!(ready["ready"], false);
    assert_eq!(ready["waiting_for"], "ollama");
    let (status, _) = send(&platform, "GET", "/_freellama/v1/health", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn unload_never_loads_a_model_that_is_not_resident() {
    let ollama = embedding_ollama(false).await;
    let platform = platform(&ollama.endpoint);
    let (status, receipt) = send(
        &platform,
        "POST",
        "/_freellama/v1/unload",
        json!({"model":"embed:1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["status"], "not_resident");
    assert_eq!(receipt["unloaded"], false);
    assert!(ollama.embeds.lock().unwrap().is_empty());
    assert_eq!(*ollama.unloads.lock().unwrap(), 0);

    ollama.resident.store(true, Ordering::SeqCst);
    let (status, receipt) = send(
        &platform,
        "POST",
        "/_freellama/v1/unload",
        json!({"model":"embed:1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["status"], "unloaded");
    assert_eq!(*ollama.unloads.lock().unwrap(), 1);
    ollama.server.abort();
}
