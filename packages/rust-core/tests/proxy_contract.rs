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
use tokio::sync::Mutex;
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
    let execution = Arc::new(Mutex::new(()));
    let proxy = app(crate::support::proxy_config("127.0.0.1:0", upstream, false)
        .with_request_timeout(std::time::Duration::from_millis(40))
        .with_max_concurrent_requests(1)
        .with_load_lock(execution.clone()))
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
        execution.try_lock().is_ok(),
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
    let config = crate::support::proxy_config("0.0.0.0:11435", "http://127.0.0.1:11434", false);
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
    let config = crate::support::proxy_config("127.0.0.1:11435", "http://127.0.0.1:11435", false);
    assert!(config.validate().is_err());
}

/// Spawns an upstream that returns 500 for the first `fail_count` requests, then 200.
async fn spawn_flaky_upstream(fail_count: usize) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    // Counts what reaches Ollama on the client's behalf. `/api/tags` is the proxy's own read of
    // the installed models (the load forecast), not a forwarded request.
    let router = Router::new().fallback(any(move |uri: axum::http::Uri| {
        let counter = counter.clone();
        async move {
            if uri.path() == "/api/tags" {
                return (StatusCode::OK, "{\"models\":[]}").into_response();
            }
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
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false);
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
    let config = crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false);
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
    let proxy = app(crate::support::proxy_config(
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
    let proxy = app(crate::support::proxy_config(
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
    let proxy = app(crate::support::proxy_config("127.0.0.1:0", upstream, false)).unwrap();
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
    let execution = Arc::new(Mutex::new(()));
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false)
        .with_request_timeout(std::time::Duration::from_millis(200))
        .with_load_lock(execution.clone());
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let started = std::time::Instant::now();
    let response = router.oneshot(request).await.unwrap();
    assert!(
        execution.try_lock().is_ok(),
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
    let execution = Arc::new(Mutex::new(()));
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_max_concurrent_requests(1)
            .with_load_lock(execution.clone()),
    )
    .unwrap();
    let request = || Request::post("/api/chat").body(Body::from("{}")).unwrap();

    let first = proxy.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(
        execution.try_lock().is_ok(),
        "a raw body that names no model can load nothing, so it holds no backend lock"
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
        execution.try_lock().is_ok(),
        "dropping the stream releases execution"
    );
    let third = proxy.oneshot(request()).await.unwrap();
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn raw_loads_wait_for_the_backend_load_lock_but_metadata_remains_available() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let execution = Arc::new(Mutex::new(()));
    let proxy = app(crate::support::proxy_config("127.0.0.1:0", upstream, false)
        .with_max_concurrent_requests(1)
        .with_load_lock(execution.clone()))
    .unwrap();
    let managed = execution.lock().await;
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = proxy
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/chat")
                    .body(Body::from(r#"{"model":"m"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method}"
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("another model load is active"),
            "{method} must be refused by the load lock, not by another check"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "refused loads never reach Ollama"
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
        .oneshot(
            Request::post("/api/chat")
                .body(Body::from(r#"{"model":"m"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(
        execution.try_lock().is_ok(),
        "EOF releases shared execution guard"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// A generation whose model cannot be resolved may still make Ollama load something (a body
/// this proxy cannot read but Ollama can). It must be refused with a typed forecast error, not
/// admitted as a zero-byte load. A well-formed JSON body naming no model loads nothing and
/// still reaches Ollama, which answers it.
#[tokio::test]
async fn an_unresolvable_generation_is_refused_with_a_typed_forecast_error() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let proxy = app(crate::support::proxy_config("127.0.0.1:0", upstream, false)).unwrap();
    let refused = proxy
        .clone()
        .oneshot(
            Request::post("/api/chat")
                .body(Body::from("not json and not multipart"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(refused.headers().contains_key("retry-after"));
    let body = axum::body::to_bytes(refused.into_body(), 4096)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], "model_forecast_unavailable");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing reached Ollama");

    let nameless = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(nameless.status(), StatusCode::OK);
    axum::body::to_bytes(nameless.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proxy_gives_up_after_max_attempts_on_persistent_failure() {
    let (upstream, calls) = spawn_flaky_upstream(usize::MAX).await;
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false);
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
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false)
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
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false)
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
    let config = crate::support::proxy_config("127.0.0.1:0", upstream, false)
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

#[tokio::test]
async fn model_file_work_and_unloads_bypass_the_load_lock() {
    let (upstream, calls) = spawn_flaky_upstream(0).await;
    let execution = Arc::new(Mutex::new(()));
    let proxy = app(crate::support::proxy_config("127.0.0.1:0", upstream, false)
        .with_max_concurrent_requests(1)
        .with_load_lock(execution.clone()))
    .unwrap();
    // A managed task is running; a pull must not be refused, nor hold the lock while it downloads.
    let managed = execution.lock().await;
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
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
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
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
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

#[tokio::test]
async fn a_swapping_hold_spares_raw_requests_for_a_resident_model() {
    use freellama::platform::resources::{
        HostResources, LowMemoryMode, ResourceGovernor, ResourcePolicy,
    };
    // Loading a large model leaves the host swapping. Requests for that loaded model must keep
    // running; a cold model, or the loaded one at another context size, waits for recovery.
    let upstream = Router::new()
        .route(
            "/api/ps",
            any(|| async {
                axum::Json(serde_json::json!({"models":[{"name":"warm:1b","context_length":4096,"digest":"recording-fixture-revision"}]}))
            }),
        )
        .route("/api/chat", any(|| async { "{}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let swap = Arc::new(AtomicUsize::new(100));
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            low_memory: LowMemoryMode::Advisory,
            sample_interval: Duration::from_millis(5),
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "swapping_fixture".into(),
            total_memory_bytes: Some(32 << 30),
            available_memory_bytes: Some(3 << 30),
            swap_out_pages: Some(swap.fetch_add(1000, Ordering::AcqRel) as u64),
            ..HostResources::default()
        },
    )
    .unwrap();
    governor.snapshot().await;
    governor.invalidate().await;
    assert!(governor.snapshot().await.holding, "fixture must hold");
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_resource_governor(governor)
            .with_hooks(Arc::new(RecordingHooks::default())),
    )
    .unwrap();
    let chat = |body: &'static str| {
        let proxy = proxy.clone();
        async move {
            proxy
                .oneshot(Request::post("/api/chat").body(Body::from(body)).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(chat(r#"{"model":"warm:1b"}"#).await, StatusCode::OK);
    assert_eq!(
        chat(r#"{"model":"warm:1b","options":{"num_ctx":4096}}"#).await,
        StatusCode::OK
    );
    assert_eq!(
        chat(r#"{"model":"cold:1b"}"#).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        chat(r#"{"model":"warm:1b","options":{"num_ctx":8192}}"#).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// Records what the proxy asks of the platform for raw generations.
#[derive(Default)]
struct RecordingHooks {
    prepared: std::sync::Mutex<Vec<(String, Option<u64>)>>,
    begun: AtomicUsize,
    served: std::sync::Mutex<Vec<(String, bool)>>,
    unloaded: std::sync::Mutex<Vec<String>>,
}

impl freellama::proxy::RawModelHooks for RecordingHooks {
    fn forecast<'a>(
        &'a self,
        _: &'a str,
        context: Option<u64>,
    ) -> freellama::proxy::ForecastFuture<'a> {
        Box::pin(async move {
            Some(freellama::proxy::RawForecast {
                required_host_bytes: 4 << 30,
                context_tokens: context.or(Some(4096)),
                digest: Some("recording-fixture-revision".into()),
            })
        })
    }
    fn prepare<'a>(
        &'a self,
        model: &'a str,
        context: Option<u64>,
    ) -> freellama::proxy::HookFuture<'a> {
        self.prepared
            .lock()
            .unwrap()
            .push((model.to_owned(), context));
        Box::pin(async {})
    }

    fn begin(&self, _model: &str) -> Box<dyn Send> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        Box::new(())
    }

    fn served(&self, model: &str, reuse_unverified: bool) {
        self.served
            .lock()
            .unwrap()
            .push((model.to_owned(), reuse_unverified));
    }

    fn unload_accepted(&self, model: &str) {
        self.unloaded.lock().unwrap().push(model.to_owned());
    }
}

#[tokio::test]
async fn raw_generations_tell_the_platform_which_model_they_load_and_use() {
    let upstream = Router::new()
        .route(
            "/api/ps",
            any(|| async {
                axum::Json(serde_json::json!({"models":[
                    {"name":"warm:1b","context_length":4096,"digest":"recording-fixture-revision"},
                    {"name":"plain:latest","context_length":4096,"digest":"recording-fixture-revision"}
                ]}))
            }),
        )
        .route("/api/chat", any(|| async { "{\"done\":true}" }))
        .route("/api/generate", any(|| async { "{\"done\":true}" }))
        .route("/api/tags", any(|| async { "{\"models\":[]}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let hooks = Arc::new(RecordingHooks::default());
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_hooks(hooks.clone()),
    )
    .unwrap();
    for body in [
        r#"{"model":"warm:1b"}"#,
        r#"{"model":"cold:1b","options":{"num_ctx":8192}}"#,
        r#"{"model":"warm:1b","options":{"num_ctx":8192}}"#,
        // A bare name means `:latest` to Ollama, never a family's only other tag.
        r#"{"model":"plain"}"#,
        r#"{"model":"warm"}"#,
    ] {
        let response = proxy
            .clone()
            .oneshot(Request::post("/api/chat").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), usize::MAX).await.unwrap();
    }
    // Metadata reads name no model to load.
    let tags = proxy
        .clone()
        .oneshot(Request::get("/api/tags").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(tags.status(), StatusCode::OK);
    let unload = proxy
        .oneshot(
            Request::post("/api/generate")
                .body(Body::from(r#"{"model":"warm:1b","keep_alive":0}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unload.status(), StatusCode::OK);
    assert_eq!(*hooks.unloaded.lock().unwrap(), vec!["warm:1b"]);
    // Only loads make room: a cold model, or a resident one at another context size.
    assert_eq!(
        *hooks.prepared.lock().unwrap(),
        vec![
            ("cold:1b".to_owned(), Some(8192)),
            ("warm:1b".to_owned(), Some(8192)),
            ("warm:latest".to_owned(), Some(4096))
        ]
    );
    assert_eq!(hooks.begun.load(Ordering::SeqCst), 5);
    assert_eq!(
        *hooks.served.lock().unwrap(),
        vec![
            ("warm:1b".to_owned(), false),
            ("cold:1b".to_owned(), true),
            ("warm:1b".to_owned(), true),
            ("plain:latest".to_owned(), false),
            ("warm:latest".to_owned(), true)
        ]
    );
}

#[tokio::test]
async fn healthy_raw_cold_loads_reserve_forecasts_until_response_release() {
    use freellama::platform::resources::{HostResources, ResourceGovernor, ResourcePolicy};
    struct ForecastHooks;
    impl freellama::proxy::RawModelHooks for ForecastHooks {
        fn prepare<'a>(&'a self, _: &'a str, _: Option<u64>) -> freellama::proxy::HookFuture<'a> {
            Box::pin(async {})
        }
        fn begin(&self, _: &str) -> Box<dyn Send> {
            Box::new(())
        }
        fn served(&self, _: &str, _: bool) {}
        fn unload_accepted(&self, _: &str) {}
        fn forecast<'a>(
            &'a self,
            _: &'a str,
            _: Option<u64>,
        ) -> freellama::proxy::ForecastFuture<'a> {
            Box::pin(async {
                Some(freellama::proxy::RawForecast {
                    required_host_bytes: 8 << 30,
                    context_tokens: Some(4096),
                    digest: Some("revision".into()),
                })
            })
        }
    }
    let upstream = Router::new()
        .route(
            "/api/ps",
            any(|| async { axum::Json(serde_json::json!({"models":[]})) }),
        )
        .route("/api/chat", any(|| async { "{\"done\":true}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let governor = ResourceGovernor::with_sampler(ResourcePolicy::default(), || HostResources {
        source: "healthy_raw_forecast_fixture".into(),
        total_memory_bytes: Some(32 << 30),
        available_memory_bytes: Some(16 << 30),
        ..HostResources::default()
    })
    .unwrap();
    assert!(!governor.snapshot().await.holding);
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_hooks(Arc::new(ForecastHooks))
            .with_resource_governor(governor.clone())
            .with_max_concurrent_requests(2)
            .with_raw_queue_wait(Duration::from_secs(5)),
    )
    .unwrap();
    let request = || {
        Request::post("/api/chat")
            .body(Body::from(r#"{"model":"cold:1b"}"#))
            .unwrap()
    };
    let first = proxy.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(governor.snapshot().await.reserved_bytes, 8 << 30);
    let second = proxy.oneshot(request()).await.unwrap();
    assert_eq!(
        second.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the healthy sample cannot admit a second overlapping forecast"
    );
    drop(first);
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    server.abort();
}

#[tokio::test]
async fn unavailable_local_model_forecast_refuses_before_inference_but_missing_model_reaches_validation()
 {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::clone(&calls);
    let upstream = Router::new()
        .route(
            "/api/ps",
            any(|| async { axum::Json(serde_json::json!({"models":[]})) }),
        )
        .route(
            "/api/tags",
            any(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/api/chat",
            any(move || {
                let requests = Arc::clone(&requests);
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::BAD_REQUEST, "missing model")
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let proxy = app(crate::support::proxy_config(
        "127.0.0.1:0",
        format!("http://{addr}"),
        false,
    ))
    .unwrap();
    let response = proxy
        .clone()
        .oneshot(
            Request::post("/api/chat")
                .body(Body::from(r#"{"model":"cold:1b"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["code"], "model_forecast_unavailable");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let response = proxy
        .oneshot(Request::post("/api/chat").body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn raw_lifecycle_waiters_use_the_existing_bounded_admission_queue() {
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct BlockedHooks {
        active: Arc<AtomicUsize>,
        waiting: Arc<AtomicUsize>,
    }
    impl freellama::proxy::RawModelHooks for BlockedHooks {
        fn prepare<'a>(&'a self, _: &'a str, _: Option<u64>) -> freellama::proxy::HookFuture<'a> {
            Box::pin(async {})
        }
        fn begin(&self, _: &str) -> Box<dyn Send> {
            self.active.fetch_add(1, Ordering::SeqCst);
            Box::new(Lease(Arc::clone(&self.active)))
        }
        fn ready<'a>(&'a self, _: &'a str) -> freellama::proxy::HookFuture<'a> {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
        fn served(&self, _: &str, _: bool) {}
        fn unload_accepted(&self, _: &str) {}
    }
    let active = Arc::new(AtomicUsize::new(0));
    let ready_calls = Arc::new(AtomicUsize::new(0));
    let admission = freellama::proxy::RawAdmission::new(1, Duration::from_secs(30));
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", "http://127.0.0.1:1", false)
            .with_raw_admission(admission.clone())
            .with_hooks(Arc::new(BlockedHooks {
                active: Arc::clone(&active),
                waiting: Arc::clone(&ready_calls),
            })),
    )
    .unwrap();
    let request = || {
        Request::post("/api/chat")
            .body(Body::from(r#"{"model":"waiting:1b"}"#))
            .unwrap()
    };
    let mut pending = Vec::new();
    for _ in 0..9 {
        let proxy = proxy.clone();
        pending.push(tokio::spawn(async move { proxy.oneshot(request()).await }));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while admission.receipt()["waiting"] != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let refused = proxy.oneshot(request()).await.unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(admission.receipt()["active"], 1);
    assert_eq!(
        ready_calls.load(Ordering::SeqCst),
        1,
        "only the slot owner waits on model lifecycle"
    );
    assert_eq!(
        active.load(Ordering::SeqCst),
        9,
        "queued requests retain their registered demand"
    );
    for request in pending {
        request.abort();
        let _ = request.await;
    }
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(admission.receipt()["active"], 0);
    assert_eq!(admission.receipt()["waiting"], 0);
}

#[tokio::test]
async fn cancelled_raw_unload_keeps_ownership_until_bounded_cleanup_confirms_absence() {
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.store(0, Ordering::SeqCst);
        }
    }
    struct Hooks(Arc<AtomicUsize>);
    impl freellama::proxy::RawModelHooks for Hooks {
        fn prepare<'a>(&'a self, _: &'a str, _: Option<u64>) -> freellama::proxy::HookFuture<'a> {
            Box::pin(async {})
        }
        fn begin(&self, _: &str) -> Box<dyn Send> {
            Box::new(())
        }
        fn served(&self, _: &str, _: bool) {}
        fn unload_accepted(&self, _: &str) {}
        fn acquire_unload<'a>(&'a self, _: &'a str) -> freellama::proxy::LeaseFuture<'a> {
            self.0.store(1, Ordering::SeqCst);
            Box::pin(async move { Box::new(Lease(Arc::clone(&self.0))) as Box<dyn Send> })
        }
    }
    let owned = Arc::new(AtomicUsize::new(0));
    let (started, dispatched) = tokio::sync::oneshot::channel();
    let started = Arc::new(std::sync::Mutex::new(Some(started)));
    let release = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::clone(&release);
    let upstream = Router::new()
        .route(
            "/api/ps",
            any(|| async { axum::Json(serde_json::json!({"models":[]})) }),
        )
        .route(
            "/api/generate",
            any(move || {
                let started = Arc::clone(&started);
                let finish = Arc::clone(&finish);
                async move {
                    if let Some(started) = started.lock().unwrap().take() {
                        let _ = started.send(());
                    }
                    finish.notified().await;
                    "{\"done\":true}"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", format!("http://{addr}"), false)
            .with_hooks(Arc::new(Hooks(Arc::clone(&owned)))),
    )
    .unwrap();
    let caller = tokio::spawn(async move {
        proxy
            .oneshot(
                Request::post("/api/generate")
                    .body(Body::from(r#"{"model":"m:latest","keep_alive":0}"#))
                    .unwrap(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), dispatched)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    let _ = caller.await;
    assert_eq!(
        owned.load(Ordering::SeqCst),
        1,
        "caller cancellation cannot release an already-dispatched unload"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while owned.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn raw_unload_waiting_for_demand_does_not_occupy_the_generation_slot() {
    struct Hooks {
        unloading: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
    }
    impl freellama::proxy::RawModelHooks for Hooks {
        fn prepare<'a>(&'a self, _: &'a str, _: Option<u64>) -> freellama::proxy::HookFuture<'a> {
            Box::pin(async {})
        }
        fn begin(&self, _: &str) -> Box<dyn Send> {
            Box::new(())
        }
        fn ready<'a>(&'a self, _: &'a str) -> freellama::proxy::HookFuture<'a> {
            self.running.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
        fn acquire_unload<'a>(&'a self, _: &'a str) -> freellama::proxy::LeaseFuture<'a> {
            self.unloading.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
        fn served(&self, _: &str, _: bool) {}
        fn unload_accepted(&self, _: &str) {}
    }
    let unloading = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(AtomicUsize::new(0));
    let admission = freellama::proxy::RawAdmission::new(1, Duration::from_secs(30));
    let proxy = app(
        crate::support::proxy_config("127.0.0.1:0", "http://127.0.0.1:1", false)
            .with_raw_admission(admission.clone())
            .with_hooks(Arc::new(Hooks {
                unloading: Arc::clone(&unloading),
                running: Arc::clone(&running),
            })),
    )
    .unwrap();
    let control = tokio::spawn({
        let proxy = proxy.clone();
        async move {
            proxy
                .oneshot(
                    Request::post("/api/generate")
                        .body(Body::from(r#"{"model":"m:latest","keep_alive":0}"#))
                        .unwrap(),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while unloading.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(admission.receipt()["active"], 0);
    let generation = tokio::spawn(async move {
        proxy
            .oneshot(
                Request::post("/api/chat")
                    .body(Body::from(
                        r#"{"model":"m:latest","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while running.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(admission.receipt()["active"], 1);
    control.abort();
    generation.abort();
    let _ = control.await;
    let _ = generation.await;
    assert_eq!(admission.receipt()["active"], 0);
}

/// A model Ollama does not list cannot load, so a raw request must reach Ollama and get its own
/// "not found". A forecast refusal there says "retry", and a client would retry forever.
#[tokio::test]
async fn a_raw_request_for_a_model_that_is_not_installed_gets_ollamas_answer() {
    let mock = axum::Router::new()
        .route(
            "/api/tags",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"models": [{"name": "real:latest", "digest": "d", "size": 1_000}]}))
            }),
        )
        .route("/api/ps", axum::routing::get(|| async { axum::Json(serde_json::json!({"models": []})) }))
        .route(
            "/api/chat",
            axum::routing::post(|| async {
                (axum::http::StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": "model 'missing:1' not found"})))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = freellama::platform::app(&crate::support::platform_config(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
    ))
    .unwrap();
    let response = tower::ServiceExt::oneshot(
        platform,
        axum::http::Request::post("/api/chat")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"model":"missing:1","messages":[],"stream":false}"#,
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("not found"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    server.abort();
}
