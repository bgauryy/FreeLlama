use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tower::ServiceExt;

mod common;

async fn call(platform: &Router, path: &str, body: Value) -> (StatusCode, Value) {
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
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn session(platform: &Router) -> String {
    let (status, receipt) = call(platform, "/_freellama/v1/sessions", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    receipt["session_id"].as_str().unwrap().to_owned()
}

fn task(id: &str, input: &str) -> Value {
    json!({"task":"embedding", "model":"embed-model", "session_id":id, "input":input})
}

async fn fixture() -> (
    Router,
    Arc<Notify>,
    Arc<Notify>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async { Json(json!({"models":[{"name":"embed-model","size":1000}]})) }),
        )
        .route(
            "/api/ps",
            get(|| async {
                Json(json!({"models":[{"name":"embed-model","size":1000,"size_vram":1000}]}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(
                    json!({"capabilities":["embedding"],"model_info":{"test.context_length":2048}}),
                )
            }),
        )
        .route(
            "/api/chat",
            post({
                let (started, release) = (started.clone(), release.clone());
                move || {
                    let (started, release) = (started.clone(), release.clone());
                    async move {
                        started.notify_one();
                        release.notified().await;
                        Json(json!({}))
                    }
                }
            }),
        )
        .route(
            "/api/embed",
            post({
                let (started, release, calls) = (started.clone(), release.clone(), calls.clone());
                move |Json(body): Json<Value>| {
                    let (started, release, calls) =
                        (started.clone(), release.clone(), calls.clone());
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if body["input"] == "hold" {
                            started.notify_one();
                            release.notified().await;
                        }
                        Json(json!({"embeddings":[[0.1]],"prompt_eval_count":1}))
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let platform = freellama::platform::app(
        &common::platform_config("127.0.0.1:11435", upstream, None, None, "embed-model")
            .with_max_concurrent_tasks(1)
            .with_max_queue_wait(Duration::from_secs(5)),
    )
    .unwrap();
    (platform, started, release, calls, server)
}

#[tokio::test]
async fn kill_cancels_active_work_releases_capacity_and_does_not_kill_other_sessions() {
    let (platform, started, release, calls, server) = fixture().await;
    let id = session(&platform).await;
    let other = session(&platform).await;
    let running = {
        let platform = platform.clone();
        let input = task(&id, "hold");
        tokio::spawn(async move { call(&platform, "/_freellama/v1/tasks", input).await })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let (status, receipt) = call(
        &platform,
        &format!("/_freellama/v1/sessions/{id}/kill"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["killed"], true);
    assert_eq!(receipt["runner_stop_confirmed"], false);
    let (status, error) = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["code"], "session_killed");
    assert_eq!(
        call(&platform, "/_freellama/v1/tasks", task(&id, "later"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&platform, "/_freellama/v1/tasks", task(&other, "other"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        call(
            &platform,
            &format!("/_freellama/v1/sessions/{id}/kill"),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    release.notify_one();
    server.abort();
}

#[tokio::test]
async fn kill_cancels_waiting_batch_items_without_dispatching_them_to_ollama() {
    let (platform, started, release, calls, server) = fixture().await;
    let holder = session(&platform).await;
    let doomed = session(&platform).await;
    let running = {
        let platform = platform.clone();
        let input = task(&holder, "hold");
        tokio::spawn(async move { call(&platform, "/_freellama/v1/tasks", input).await })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let batch = {
        let platform = platform.clone();
        let input = json!({"max_parallelism":1, "tasks":[
            {"id":"queued", "independent":true, "task":task(&doomed,"queued")},
            {"id":"pending", "independent":true, "task":task(&doomed,"pending")}
        ]});
        tokio::spawn(async move { call(&platform, "/_freellama/v1/task-batches", input).await })
    };
    // Observe admission rather than guessing that a timer means the request is queued.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = platform
                .clone()
                .oneshot(
                    Request::get("/_freellama/v1/health")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let health: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if health["backends"]["gpu"]["admission"]["queue_depth"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        call(
            &platform,
            &format!("/_freellama/v1/sessions/{doomed}/kill"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, result) = tokio::time::timeout(Duration::from_secs(2), batch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["ok"] == false)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    assert_eq!(running.await.unwrap().0, StatusCode::OK);
    server.abort();
}

#[tokio::test]
async fn kill_also_cancels_session_scoped_intent_inference() {
    let (platform, started, release, _, server) = fixture().await;
    let id = session(&platform).await;
    let running = {
        let platform = platform.clone();
        let input = json!({"text":"Summarize a paragraph", "session_id":id});
        tokio::spawn(async move { call(&platform, "/_freellama/v1/natural-routes", input).await })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    assert_eq!(
        call(
            &platform,
            &format!("/_freellama/v1/sessions/{id}/kill"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap()
            .1["code"],
        "session_killed"
    );
    release.notify_one();
    server.abort();
}
