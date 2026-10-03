use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use freellama::proxy::{app, proxy_target};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tower::ServiceExt;

struct IncompleteRequestBody;

impl http_body::Body for IncompleteRequestBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Pending
    }
}

#[tokio::test]
async fn incomplete_upload_times_out_and_releases_execution_before_any_upstream_request() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let execution = Arc::new(RwLock::new(()));
    let proxy = app(common::proxy_config("127.0.0.1:0", upstream, false)
        .with_request_timeout(std::time::Duration::from_millis(40))
        .with_max_concurrent_requests(1)
        .with_execution_lock(execution.clone()))
    .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        proxy.clone().oneshot(
            Request::post("/api/chat")
                .body(Body::new(IncompleteRequestBody))
                .unwrap(),
        ),
    )
    .await
    .expect("an incomplete client upload cannot hold execution indefinitely")
    .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        execution.try_write().is_ok(),
        "timeout releases the backend lock before its error body is consumed"
    );
    let healthy = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(
        healthy.status(),
        StatusCode::OK,
        "timeout releases raw admission too"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A fake restart action that records how many times it was called instead of touching a real
/// system process — lets the retry-then-restart-then-retry-once-more orchestration be verified
/// deterministically.
fn counting_restart_action(calls: Arc<AtomicUsize>) -> freellama::proxy::RestartAction {
    Arc::new(move || {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
        })
    })
}

#[test]
fn proxy_is_loopback_only_by_default() {
    let config = common::proxy_config("0.0.0.0:11435", "http://127.0.0.1:11434", false);
    assert!(config.validate().is_err());
}

#[test]
fn proxy_preserves_path_and_query() {
    let target = proxy_target("http://127.0.0.1:11434/", "/api/chat?trace=one%20two").unwrap();
    assert_eq!(
        target.as_str(),
        "http://127.0.0.1:11434/api/chat?trace=one%20two"
    );
}

#[test]
fn proxy_rejects_a_recursive_upstream() {
    let config = common::proxy_config("127.0.0.1:11435", "http://127.0.0.1:11435", false);
    assert!(config.validate().is_err());
}

/// Spawns an upstream that returns 500 for the first `fail_count` requests, then 200.
async fn spawn_flaky_upstream(fail_count: usize) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(any(move |State(()): State<()>| {
        let counter = counter.clone();
        async move {
            let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if seen <= fail_count {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "transient upstream error",
                )
                    .into_response()
            } else {
                (StatusCode::OK, "{\"ok\":true}").into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), calls)
}

#[tokio::test]
async fn proxy_retries_transient_upstream_errors_and_eventually_succeeds() {
    let (upstream, calls) = spawn_flaky_upstream(2).await;
    let config = common::proxy_config("127.0.0.1:0", upstream, false);
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected 2 failed attempts + 1 successful attempt"
    );
}

/// 503 is "busy, shed load" — retrying it through the passthrough would amplify the same
/// saturation the managed path already refuses to retry.
#[tokio::test]
async fn proxy_does_not_retry_upstream_503() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(any(move |State(()): State<()>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = common::proxy_config("127.0.0.1:0", format!("http://{addr}"), false);
    let proxy = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = proxy.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "503 must not be retried on the passthrough proxy"
    );
}

/// Only the documented transient status allowlist is retryable. A generic server error such as
/// 501 is usually permanent for this request and must not be multiplied three times.
#[tokio::test]
async fn proxy_does_not_retry_an_unlisted_5xx_status() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(any(move |State(()): State<()>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            (StatusCode::NOT_IMPLEMENTED, "unsupported").into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let proxy = app(common::proxy_config(
        "127.0.0.1:0",
        format!("http://{addr}"),
        false,
    ))
    .unwrap();

    let response = proxy
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A gateway can time out while its Ollama request is still running. An HTTP timeout response
/// has the same ambiguous completion boundary as a client timeout, so it must not be replayed.
#[tokio::test]
async fn proxy_does_not_retry_upstream_504() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(any(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            (StatusCode::GATEWAY_TIMEOUT, "upstream timed out")
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let proxy = app(common::proxy_config(
        "127.0.0.1:0",
        format!("http://{addr}"),
        false,
    ))
    .unwrap();
    let response = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn proxy_does_not_replay_after_upstream_accepts_then_disconnects() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            server_calls.fetch_add(1, Ordering::SeqCst);
            // The server received the request and may have begun generation. Losing the response
            // does not establish that replaying the request is safe.
            drop(socket);
        }
    });
    let proxy = app(common::proxy_config("127.0.0.1:0", upstream, false)).unwrap();
    let response = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

/// Spawns an upstream that accepts the connection but never responds (holds it open past
/// `hang_for`), to exercise the proxy's per-request timeout independent of retry logic.
async fn spawn_hanging_upstream(hang_for: std::time::Duration) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let router = Router::new().fallback(any(move |State(()): State<()>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(hang_for).await;
            (StatusCode::OK, "{\"ok\":true}").into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), calls)
}

#[tokio::test]
async fn proxy_times_out_a_hung_upstream_instead_of_blocking_forever() {
    let (upstream, calls) = spawn_hanging_upstream(std::time::Duration::from_secs(30)).await;
    let execution = Arc::new(RwLock::new(()));
    let config = common::proxy_config("127.0.0.1:0", upstream, false)
        .with_request_timeout(std::time::Duration::from_millis(200))
        .with_execution_lock(execution.clone());
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let started = std::time::Instant::now();
    let response = router.oneshot(request).await.unwrap();
    assert!(
        execution.try_write().is_ok(),
        "upstream errors must release shared execution before returning"
    );
    let elapsed = started.elapsed();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a timed-out generation may still be running upstream and must not be duplicated"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "expected the single 200ms attempt to bound total wait, took {elapsed:?}"
    );
}

/// The raw cap protects Ollama executions, not merely the brief interval until upstream response
/// headers arrive. A streaming generation still consumes the runner after headers, so its permit
/// must remain held until the downstream body is dropped or reaches EOF.
#[tokio::test]
async fn raw_proxy_cap_is_held_for_the_stream_lifetime() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let seen = server_calls.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                if seen == 0 {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n",
                        )
                        .await
                        .unwrap();
                    std::future::pending::<()>().await;
                } else {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                        .await
                        .unwrap();
                }
            });
        }
    });
    let execution = Arc::new(RwLock::new(()));
    let proxy = app(
        common::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_max_concurrent_requests(1)
            .with_execution_lock(execution.clone()),
    )
    .unwrap();
    let request = || Request::post("/api/chat").body(Body::from("{}")).unwrap();

    let first = proxy.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(
        execution.try_read().is_err(),
        "raw stream must exclude managed execution"
    );
    let second = proxy.clone().oneshot(request()).await.unwrap();
    // The configured raw cap is FreeLlama's own limit: 429 with a Retry-After hint.
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().contains_key("retry-after"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the refused request must not reach Ollama while a response stream is live"
    );

    drop(first);
    assert!(
        execution.try_write().is_ok(),
        "dropping the stream releases execution"
    );
    let third = proxy.oneshot(request()).await.unwrap();
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn raw_mutations_share_managed_execution_but_metadata_remains_available() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let execution = Arc::new(RwLock::new(()));
    let proxy = app(common::proxy_config("127.0.0.1:0", upstream, false)
        .with_max_concurrent_requests(1)
        .with_execution_lock(execution.clone()))
    .unwrap();
    let managed = execution.read().await;
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = proxy
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/chat")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method}"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "refused mutations never reach Ollama"
    );
    for method in ["GET", "HEAD"] {
        let response = proxy
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/ps")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
    }
    drop(managed);
    let response = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(
        execution.try_write().is_ok(),
        "EOF releases shared execution guard"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn proxy_gives_up_after_max_attempts_on_persistent_failure() {
    let (upstream, calls) = spawn_flaky_upstream(usize::MAX).await;
    let config = common::proxy_config("127.0.0.1:0", upstream, false);
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected exactly MAX_ATTEMPTS tries, no more"
    );
}

/// A closed TCP port (nothing listening) reproduces exactly what "Ollama process is dead" looks
/// like to a client: connection refused, not a slow response or an HTTP error. Bind then
/// immediately drop a listener to get a real, guaranteed-unused port instead of a magic number.
async fn closed_port_upstream() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

#[tokio::test]
async fn proxy_restarts_ollama_once_after_a_connection_refused_failure() {
    let upstream = closed_port_upstream().await;
    let restart_calls = Arc::new(AtomicUsize::new(0));
    let config = common::proxy_config("127.0.0.1:0", upstream, false)
        .with_auto_restart_ollama(true)
        .with_restart_action(counting_restart_action(restart_calls.clone()));
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    // Nothing is actually listening, so the request still ultimately fails — restarting a fake
    // action doesn't make a closed port answer. What this proves is the orchestration: exactly
    // one restart attempt, not zero (feature works) and not a restart storm (cooldown works).
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        restart_calls.load(Ordering::SeqCst),
        1,
        "expected exactly one restart attempt for one failed request"
    );
}

#[tokio::test]
async fn proxy_does_not_restart_ollama_when_auto_restart_is_disabled() {
    let upstream = closed_port_upstream().await;
    let restart_calls = Arc::new(AtomicUsize::new(0));
    // auto_restart_ollama defaults to false — the restart action is wired up but must never fire.
    let config = common::proxy_config("127.0.0.1:0", upstream, false)
        .with_restart_action(counting_restart_action(restart_calls.clone()));
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        restart_calls.load(Ordering::SeqCst),
        0,
        "opt-in flag is off by default — must never restart without it"
    );
}

#[tokio::test]
async fn proxy_does_not_restart_ollama_for_an_ordinary_5xx_not_a_dead_process() {
    let (upstream, calls) = spawn_flaky_upstream(usize::MAX).await;
    let restart_calls = Arc::new(AtomicUsize::new(0));
    let config = common::proxy_config("127.0.0.1:0", upstream, false)
        .with_auto_restart_ollama(true)
        .with_restart_action(counting_restart_action(restart_calls.clone()));
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "a live-but-erroring upstream must still only get the normal MAX_ATTEMPTS retries"
    );
    assert_eq!(
        restart_calls.load(Ordering::SeqCst),
        0,
        "a real HTTP 500 is not a dead process — restarting Ollama would not help and must not happen"
    );
}
mod common;

#[tokio::test]
async fn model_file_work_and_unloads_bypass_the_execution_lock() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let execution = Arc::new(RwLock::new(()));
    let proxy = app(common::proxy_config("127.0.0.1:0", upstream, false)
        .with_max_concurrent_requests(1)
        .with_execution_lock(execution.clone()))
    .unwrap();
    // A managed task is running; a pull must not be refused, nor hold the lock while it downloads.
    let managed = execution.read().await;
    for (path, body) in [
        ("/api/pull", r#"{"model":"llama3.2:1b"}"#),
        ("/api/delete", r#"{"model":"llama3.2:1b"}"#),
        ("/api/copy", r#"{"source":"a","destination":"b"}"#),
        ("/api/blobs/sha256:abc", "bytes"),
        (
            "/api/chat",
            r#"{"model":"m","messages":[],"keep_alive":"0m"}"#,
        ),
        ("/api/generate", r#"{"model":"m","keep_alive":0}"#),
    ] {
        let response = proxy
            .clone()
            .oneshot(Request::post(path).body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    // A generation still waits for managed execution to finish.
    let generation = proxy
        .clone()
        .oneshot(
            Request::post("/api/chat")
                .body(Body::from(r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"keep_alive":"0m"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(generation.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(managed);
}

#[tokio::test]
async fn request_timeout_bounds_silence_not_a_stream_that_keeps_producing() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        // Six chunks 60ms apart: 360ms in total, far past the 150ms timeout, never silent for it.
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            socket.write_all(b"3\r\n{}\n\r\n").await.unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let proxy = app(
        common::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_request_timeout(std::time::Duration::from_millis(150)),
    )
    .unwrap();
    let response = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a live stream is not cut off by the idle timeout");
    assert_eq!(body.len(), 18);
}

/// With a raw queue wait, a request that finds the cap full waits for the live stream to end
/// instead of being refused at once, and still never overlaps it upstream.
#[tokio::test]
async fn raw_queue_wait_admits_a_waiter_when_the_stream_ends() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_calls.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await;
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
            });
        }
    });
    let proxy = app(
        common::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_max_concurrent_requests(1)
            .with_raw_queue_wait(Duration::from_secs(2)),
    )
    .unwrap();
    let request = || Request::post("/api/chat").body(Body::from("{}")).unwrap();
    let first = proxy.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let waiter = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.oneshot(request()).await.unwrap().status() }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the waiter must not overlap the stream"
    );
    to_bytes(first.into_body(), usize::MAX).await.unwrap();
    assert_eq!(waiter.await.unwrap(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
