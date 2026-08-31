use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use freellama::proxy::{ProxyConfig, app, proxy_target};
use tower::ServiceExt;

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
    let config = ProxyConfig::new("0.0.0.0:11435", "http://127.0.0.1:11434", false);
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
    let config = ProxyConfig::new("127.0.0.1:11435", "http://127.0.0.1:11435", false);
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
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false);
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

/// Spawns an upstream that accepts the connection but never responds (holds it open past
/// `hang_for`), to exercise the proxy's per-request timeout independent of retry logic.
async fn spawn_hanging_upstream(hang_for: std::time::Duration) -> String {
    let router = Router::new().fallback(any(move |State(()): State<()>| async move {
        tokio::time::sleep(hang_for).await;
        (StatusCode::OK, "{\"ok\":true}").into_response()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn proxy_times_out_a_hung_upstream_instead_of_blocking_forever() {
    let upstream = spawn_hanging_upstream(std::time::Duration::from_secs(30)).await;
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false)
        .with_request_timeout(std::time::Duration::from_millis(200));
    let router = app(config).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .body(Body::from("{}"))
        .unwrap();
    let started = std::time::Instant::now();
    let response = router.oneshot(request).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "expected the timeout (200ms x up to 3 attempts) to bound total wait, took {elapsed:?}"
    );
}

#[tokio::test]
async fn proxy_gives_up_after_max_attempts_on_persistent_failure() {
    let (upstream, calls) = spawn_flaky_upstream(usize::MAX).await;
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false);
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
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false)
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
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false)
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
    let config = ProxyConfig::new("127.0.0.1:0", upstream, false)
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
