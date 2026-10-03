use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

type Started = (String, oneshot::Sender<()>);

async fn controlled_platform(
    queue_wait: Duration,
) -> (
    Router,
    mpsc::UnboundedReceiver<Started>,
    tokio::task::JoinHandle<()>,
) {
    let (send, receive) = mpsc::unbounded_channel::<Started>();
    let send = Arc::new(send);
    let handler = move |Json(body): Json<Value>| {
        let send = Arc::clone(&send);
        async move {
            let label = body
                .get("input")
                .and_then(Value::as_str)
                .or_else(|| body.pointer("/messages/0/content").and_then(Value::as_str))
                .unwrap_or("raw")
                .to_owned();
            let (release, released) = oneshot::channel();
            send.send((label, release)).unwrap();
            let _ = released.await;
            Json(json!({"message":{"content":"ok"},"embeddings":[[0.1]],"done":true}))
        }
    };
    let upstream = Router::new()
        .route("/api/tags", get(|| async {Json(json!({"models":[{"name":"test:latest","size":1}]}))}))
        .route("/api/ps", get(|| async {Json(json!({"models":[{"name":"test:latest","size":1,"size_vram":1}]}))}))
        .route("/api/show", post(|| async {Json(json!({"capabilities":["completion","embedding"],"model_info":{"general.architecture":"llama","llama.context_length":32768}}))}))
        .route("/api/chat", post(handler.clone()))
        .route("/api/embed", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        format!("http://{address}"),
        None,
        None,
        "test:latest",
    )
    .with_max_concurrent_tasks(2)
    .with_max_queue_wait(queue_wait))
    .unwrap();
    (platform, receive, server)
}

fn task(platform: &Router, label: &str, large: bool) -> tokio::task::JoinHandle<StatusCode> {
    let platform = platform.clone();
    let input = if large {
        json!({"task":"completion","model":"test:latest","objective":"fastest","priority":"background","prompt":label})
    } else {
        json!({"task":"embedding","model":"test:latest","objective":"fastest","priority":"interactive","input":label})
    };
    tokio::spawn(async move {
        platform
            .oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    })
}

async fn health(platform: &Router) -> Value {
    let response = platform
        .clone()
        .oneshot(
            Request::get("/_freellama/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn wait_for_queue(platform: &Router, expected: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if health(platform).await["backends"]["gpu"]["admission"]["queue_depth"] == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("requests must reach the managed wait queue");
}

async fn started(
    receive: &mut mpsc::UnboundedReceiver<Started>,
    expected: &str,
) -> oneshot::Sender<()> {
    let (label, release) = tokio::time::timeout(Duration::from_secs(2), receive.recv())
        .await
        .expect("expected request to start")
        .unwrap();
    assert_eq!(label, expected);
    release
}

async fn exercise_bounded_bypass(cancel_reserved: bool) {
    let (platform, mut receive, server) = controlled_platform(Duration::from_secs(10)).await;
    let anchor = task(&platform, "anchor", false);
    let release_anchor = started(&mut receive, "anchor").await;
    let large = task(&platform, "large", true);
    wait_for_queue(&platform, 1).await;

    // The anchor occupies one of two units. Small interactive jobs can use the other unit,
    // while the older background generation needs both. A bounded number may pass it.
    for index in 0..6 {
        let label = format!("small-{index}");
        let small = task(&platform, &label, false);
        started(&mut receive, &label).await.send(()).unwrap();
        assert_eq!(small.await.unwrap(), StatusCode::OK);
    }
    let following = task(&platform, "following", false);
    wait_for_queue(&platform, 2).await;
    assert!(
        receive.try_recv().is_err(),
        "capacity must now drain for the older large task"
    );

    if cancel_reserved {
        large.abort();
        assert!(large.await.unwrap_err().is_cancelled());
        started(&mut receive, "following").await.send(()).unwrap();
        assert_eq!(following.await.unwrap(), StatusCode::OK);
        assert_eq!(
            health(&platform).await["backends"]["gpu"]["admission"]["queue_cancellations"],
            1
        );
        release_anchor.send(()).unwrap();
    } else {
        release_anchor.send(()).unwrap();
        let release_large = started(&mut receive, "large").await;
        assert!(
            receive.try_recv().is_err(),
            "large task owns the full capacity"
        );
        release_large.send(()).unwrap();
        assert_eq!(large.await.unwrap(), StatusCode::OK);
        started(&mut receive, "following").await.send(()).unwrap();
        assert_eq!(following.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(anchor.await.unwrap(), StatusCode::OK);
    let admission = health(&platform).await["backends"]["gpu"]["admission"].clone();
    assert_eq!(admission["slots_available"], 2);
    assert_eq!(admission["queue_depth"], 0);
    server.abort();
}

#[tokio::test]
async fn large_background_work_cannot_be_starved_by_small_interactive_work() {
    exercise_bounded_bypass(false).await;
}

#[tokio::test]
async fn cancelling_a_capacity_reservation_unblocks_smaller_work() {
    exercise_bounded_bypass(true).await;
}

#[tokio::test]
async fn raw_execution_cannot_extend_managed_or_intent_queue_deadlines() {
    let (platform, mut receive, server) = controlled_platform(Duration::from_millis(80)).await;
    let raw_platform = platform.clone();
    let raw = tokio::spawn(async move {
        raw_platform
            .oneshot(
                Request::post("/api/chat")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    let release_raw = started(&mut receive, "raw").await;
    for path in ["/_freellama/v1/tasks", "/_freellama/v1/natural-routes"] {
        let input = if path.ends_with("natural-routes") {
            json!({"text":"answer a short question"})
        } else {
            json!({"task":"embedding","model":"test:latest","objective":"fastest","input":"blocked"})
        };
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            platform.clone().oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            ),
        )
        .await
        .expect("transition waiting must use the configured queue deadline")
        .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("transition"));
        assert_eq!(
            health(&platform).await["backends"]["gpu"]["admission"]["slots_available"],
            2
        );
        assert!(
            receive.try_recv().is_err(),
            "timed out work must not reach upstream"
        );
    }
    release_raw.send(()).unwrap();
    let response = raw.await.unwrap();
    to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let followup = task(&platform, "after-timeout", false);
    started(&mut receive, "after-timeout")
        .await
        .send(())
        .unwrap();
    assert_eq!(followup.await.unwrap(), StatusCode::OK);
    server.abort();
}
mod common;
