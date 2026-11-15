//! Real HTTP responses through the managed pipeline, with small injected byte limits.
use super::*;
use crate::platform::{
    PlatformConfig, build, execution,
    resources::{HostResources, ResourceGovernor, ResourcePolicy},
    task_stream,
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::State,
    routing::{get, post},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Fixture {
    state: PlatformState,
    posts: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(payload: String, status: StatusCode, limit: usize) -> Fixture {
    let posts = Arc::new(AtomicUsize::new(0));
    let captured = posts.clone();
    let generated = move || {
        let payload = payload.clone();
        let captured = captured.clone();
        async move {
            captured.fetch_add(1, Ordering::SeqCst);
            (status, payload)
        }
    };
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models":[{
                    "name":"bounded:latest","digest":"bounded-digest","size":1000
                }]}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities":["completion"],
                    "model_info":{"general.architecture":"llama","llama.context_length":8192}
                }))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/chat", post(generated.clone()))
        .route("/api/embed", post(generated.clone()))
        .route("/v1/systemone", post(generated));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let mut config = PlatformConfig::new("127.0.0.1:0", url, None, None);
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), || HostResources {
            total_memory_bytes: Some(64 << 30),
            available_memory_bytes: Some(48 << 30),
            ..HostResources::default()
        })
        .unwrap();
    let (_, mut state) = build(&config).unwrap();
    state.max_request_bytes = limit;
    Fixture {
        state,
        posts,
        server,
    }
}

fn task(stream: bool) -> crate::platform::TaskInput {
    serde_json::from_value(json!({"model":"bounded:latest", "prompt":"hello",
        "stream":stream, "stop_on_repetition":false, "context_tokens":4096,
        "timeout_seconds":3, "request_options":{"options":{"num_predict":8}},
        "fallback_models":["never-run:latest"]}))
    .unwrap()
}

async fn assert_released(fixture: &Fixture) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if execution::primary_target(&fixture.state)
                .admission
                .receipt()["in_flight"]
                == 0
                && fixture.state.resources.snapshot().await.reserved_bytes == 0
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("execution and memory reservations released");
    assert!(
        fixture.state.load_lock.try_lock().is_ok(),
        "load lock released"
    );
}

#[tokio::test]
async fn oversized_json_and_error_bodies_fail_without_replay_and_release_capacity() {
    for (status, payload) in [
        (
            StatusCode::OK,
            json!({"done":true,"message":{"content":"x".repeat(2048)}}).to_string(),
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "upstream diagnostic ".repeat(128),
        ),
    ] {
        let fixture = fixture(payload, status, 1024).await;
        let error = execution::execute_task(State(fixture.state.clone()), Json(task(false)))
            .await
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.code(), Some("upstream_response_too_large"));
        let receipt = error.into_batch_result(String::new());
        assert_eq!(receipt["retry_safe"], false);
        assert_eq!(receipt["execution_outcome"], "unknown");
        assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
        assert_released(&fixture).await;
    }
}

#[tokio::test]
async fn total_stream_bytes_and_unterminated_frames_are_bounded() {
    let line = format!("{}\n", json!({"message":{"content":"small"},"done":false}));
    for payload in [
        format!("{}{{\"done\":true}}\n", line.repeat(100)),
        "x".repeat(2048),
    ] {
        let fixture = fixture(payload, StatusCode::OK, 1024).await;
        let error = post_streaming_chat_with_retries(
            &fixture.state,
            &fixture.state.upstream,
            "/api/chat",
            &json!({"stream":true}),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), Some("upstream_response_too_large"));
        assert!(error.followed_execution());
        assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn embeddings_and_decisions_use_the_same_buffer_limit() {
    for (path, payload) in [
        (
            "/api/embed",
            json!({"embeddings":[vec![0.125; 1024]]}).to_string(),
        ),
        (
            "/v1/systemone",
            json!({"answers":{"text":"x".repeat(2048)}}).to_string(),
        ),
    ] {
        let fixture = fixture(payload, StatusCode::OK, 1024).await;
        let error = post_json_with_retries(
            &fixture.state,
            &fixture.state.upstream,
            path,
            &json!({"model":"bounded:latest"}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), Some("upstream_response_too_large"));
        assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_response_at_the_byte_limit_is_preserved() {
    let payload = json!({"done":true,"message":{"content":"exact"}}).to_string();
    let fixture = fixture(payload.clone(), StatusCode::OK, payload.len()).await;
    let (status, value) = post_json_with_retries(
        &fixture.state,
        &fixture.state.upstream,
        "/api/chat",
        &json!({}),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["message"]["content"], "exact");
}

async fn blocked_stream() -> (Fixture, axum::response::Response) {
    let line = format!(
        "{}\n",
        json!({"message":{"content":"a piece"},"done":false})
    );
    let fixture = fixture(
        format!("{}{{\"done\":true}}\n", line.repeat(50_000)),
        StatusCode::OK,
        8 << 20,
    )
    .await;
    let mut input = task(true);
    input.timeout_seconds = Some(1);
    let response = task_stream::respond(fixture.state.clone(), input);
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.posts.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("generation starts");
    (fixture, response)
}

#[tokio::test]
async fn a_slow_connected_reader_cannot_suspend_the_execution_deadline() {
    let (fixture, response) = blocked_stream().await;
    // Do not poll the response body: both bounded hops fill and backpressure generation.
    assert_released(&fixture).await;
    let body = to_bytes(response.into_body(), 8 << 20).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("task_deadline_exceeded"),
        "reader stalls must not suspend execution deadlines"
    );
    assert!(text.contains("\"retry_safe\":false"));
    assert!(!text.contains("event: done"));
    assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_a_backpressured_reader_aborts_the_worker_and_releases_capacity() {
    let (fixture, response) = blocked_stream().await;
    drop(response);
    assert_released(&fixture).await;
    assert_eq!(fixture.posts.load(Ordering::SeqCst), 1);
}
