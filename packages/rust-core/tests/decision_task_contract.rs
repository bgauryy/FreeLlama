//! Decision models as managed work: Ollama reports them with the `decision` capability, they answer
//! typed questions through `/v1/systemone`, and they never chat. A `decision` task routes to one,
//! sends only what `/v1/systemone` reads, and refuses chat fields instead of dropping them.
use std::sync::{Arc, Mutex};

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

struct Ollama {
    decisions: Arc<Mutex<Vec<Value>>>,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

/// `decide:1` reports only `decision`; `/v1/systemone` captures each body, and `/api/chat` and
/// `/api/embed` fail, because a decision model is never sent to them.
async fn decision_ollama() -> Ollama {
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let captured = decisions.clone();
    let refuse = || async {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"decision model sent to chat or embed"})),
        )
    };
    let upstream = Router::new()
        .route("/api/version", get(|| async { Json(json!({"version":"0.40.13"})) }))
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models":[{"name":"decide:1","digest":"d","size":1_300_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route(
            "/api/show",
            post(|| async { Json(json!({"capabilities":["decision"],"model_info":{}})) }),
        )
        .route(
            "/v1/systemone",
            post(move |Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push(body);
                    Json(json!({
                        "model":"decide:1",
                        "answers":{"kind":{"type":"choice","choice":"bug","probabilities":{"bug":0.97,"feature":0.03},"confidence":0.94}},
                        "usage":{"input_tokens":174,"output_tokens":1}
                    }))
                }
            }),
        )
        .route("/api/chat", post(refuse))
        .route("/api/embed", post(refuse));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    Ollama {
        decisions,
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

fn question() -> Value {
    json!({"kind":{"type":"choice","instructions":"What kind of issue is this?",
        "criteria":{"bug":"Something is broken","feature":"A request for new capability"}}})
}

#[tokio::test]
async fn a_decision_model_is_listed_as_one() {
    let ollama = decision_ollama().await;
    let platform = platform(&ollama.endpoint);
    let (status, models) = send(&platform, "GET", "/_freellama/v1/models", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    let model = &models["models"][0];
    assert_eq!(model["model_type"], "decision", "{model}");
    assert_eq!(model["capabilities"], json!(["decision"]), "{model}");
    ollama.server.abort();
}

#[tokio::test]
async fn a_decision_task_routes_to_the_decision_model_and_sends_only_what_systemone_reads() {
    let ollama = decision_ollama().await;
    let platform = platform(&ollama.endpoint);
    let (status, result) = send(
        &platform,
        "POST",
        "/_freellama/v1/tasks",
        json!({"task":"decision","state":"Checkout returns 500 since 9am.","questions":question()}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["route"]["selected_model"], "decide:1", "{result}");
    assert_eq!(result["response"]["answers"]["kind"]["choice"], "bug");
    let sent = ollama.decisions.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let body = sent[0].as_object().unwrap();
    let mut fields = body.keys().cloned().collect::<Vec<_>>();
    fields.sort();
    assert_eq!(
        fields,
        ["keep_alive", "model", "questions", "state"],
        "{body:?}"
    );
    assert_eq!(body["state"], "Checkout returns 500 since 9am.");
    assert_eq!(body["questions"], question());
    ollama.server.abort();
}

#[tokio::test]
async fn a_decision_task_refuses_what_it_cannot_send() {
    let ollama = decision_ollama().await;
    let platform = platform(&ollama.endpoint);
    for (body, needle) in [
        (json!({"task":"decision","state":"s"}), "requires questions"),
        (
            json!({"task":"decision","questions":question()}),
            "requires a non-empty state",
        ),
        (
            json!({"task":"decision","state":"s","questions":question(),"prompt":"hi"}),
            "prompt, messages, input, and tools",
        ),
        (
            json!({"task":"decision","state":"s","questions":question(),
                "request_options":{"format":"json"}}),
            "accepts no options",
        ),
    ] {
        let (status, error) = send(&platform, "POST", "/_freellama/v1/tasks", body.clone()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} -> {error}"
        );
        assert!(
            error["error"].as_str().unwrap_or_default().contains(needle),
            "{body} -> {error}"
        );
    }
    assert!(
        ollama.decisions.lock().unwrap().is_empty(),
        "a refused task never reaches Ollama"
    );
    ollama.server.abort();
}

#[tokio::test]
async fn a_chat_task_never_routes_to_a_decision_model() {
    let ollama = decision_ollama().await;
    let platform = platform(&ollama.endpoint);
    let (status, error) = send(
        &platform,
        "POST",
        "/_freellama/v1/tasks",
        json!({"task":"completion","prompt":"hello"}),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{error}");
    assert!(ollama.decisions.lock().unwrap().is_empty());
    ollama.server.abort();
}
