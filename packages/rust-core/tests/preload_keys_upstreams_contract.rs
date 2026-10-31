//! Startup and predicted preloads, client keys with budgets, OpenAI-compatible upstreams, raw
//! usage accounting and thermal holds, end to end against mock engines.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    ClientKey, PlatformConfig, app,
    resources::{HostResources, LowMemoryMode, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use freellama::proxy::OpenAiUpstream;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Mock {
    loaded: Arc<Mutex<Vec<Value>>>,
    chats: Arc<Mutex<Vec<Value>>>,
}

fn sample(thermal: bool) -> HostResources {
    HostResources {
        source: "injected_contract".into(),
        total_memory_bytes: Some(1 << 40),
        available_memory_bytes: Some(1 << 39),
        memory_pressure: Some(MemoryPressure::Normal),
        thermal_throttled: Some(thermal),
        // An accelerator: on a CPU-only host every request heats the same cores and waits.
        gpu_memory_total_bytes: Some(1 << 36),
        gpu_memory_free_bytes: Some(1 << 35),
        gpu_telemetry_source: Some("injected".into()),
        ..HostResources::default()
    }
}

fn governor(thermal: bool) -> ResourceGovernor {
    ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        move || sample(thermal),
    )
    .unwrap()
}

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, server)
}

/// An Ollama with models `a` and `b` that reports token counts on every chat.
async fn ollama(mock: Mock) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [
                    {"name": "a:latest", "digest": "d-a", "size": 1000},
                    {"name": "b:latest", "digest": "d-b", "size": 1000}
                ]}))
            }),
        )
        .route(
            "/api/ps",
            get(|State(mock): State<Mock>| async move {
                Json(json!({"models": mock.loaded.lock().await.clone()}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({"capabilities": ["completion"],
                    "model_info": {"general.architecture": "llama", "llama.context_length": 8192}}))
            }),
        )
        .route(
            "/api/chat",
            post(
                |State(mock): State<Mock>, Json(body): Json<Value>| async move {
                    mock.chats.lock().await.push(body);
                    Json(
                        json!({"message": {"role": "assistant", "content": "ok"}, "done": true,
                    "done_reason": "stop", "prompt_eval_count": 8, "prompt_eval_cached_count": 2,
                    "eval_count": 5, "load_duration": 1_500_000_000_u64}),
                    )
                },
            ),
        )
        .route(
            "/v1/models",
            get(|| async { Json(json!({"object": "list", "data": [{"id": "a:latest"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                Json(json!({"served_by": "ollama", "model": body["model"]}))
            }),
        )
        .with_state(mock);
    serve(router).await
}

fn config(upstream: String, thermal: bool) -> PlatformConfig {
    let mut config = PlatformConfig::new("127.0.0.1:0", upstream, None, None)
        .with_max_queue_wait(Duration::from_millis(800));
    config.resource_governor = governor(thermal);
    config
}

async fn call(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

fn post_json(path: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut request = Request::post(path).header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(Body::from(body.to_string())).unwrap()
}

fn task(model: &str) -> Value {
    json!({"task": "completion", "objective": "fastest", "model": model, "prompt": "hi"})
}

async fn wait_for(mut done: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..200 {
        if done().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn runtime_file(text: &str) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), text).unwrap();
    file
}

/// A startup entry with messages evaluates them into the prompt cache without generating.
#[tokio::test]
async fn startup_preload_loads_listed_models_and_fills_their_prompt_cache() {
    let mock = Mock::default();
    let (upstream, server) = ollama(mock.clone()).await;
    let file = runtime_file(
        "[preload]\nstartup = [\"a:latest\", { model = \"b:latest\", messages = [{ role = \"system\", content = \"You review Rust.\" }] }]\n",
    );
    let platform = tokio::spawn(freellama::platform::serve(
        config(upstream, false).with_runtime_config(file.path()),
    ));
    let chats = mock.chats.clone();
    assert!(
        wait_for(async || chats.lock().await.len() >= 2).await,
        "both preloads ran"
    );
    let chats = mock.chats.lock().await.clone();
    assert_eq!(chats[0]["model"], "a:latest");
    assert_eq!(chats[0]["messages"], json!([]));
    assert_eq!(chats[1]["model"], "b:latest");
    assert_eq!(chats[1]["messages"][0]["content"], "You review Rust.");
    assert_eq!(chats[1]["options"]["num_predict"], 0, "{}", chats[1]);
    assert!(
        chats[0]["keep_alive"]
            .as_str()
            .is_some_and(|value| value.ends_with('s'))
    );
    platform.abort();
    server.abort();
}

/// After a clear pattern of switching from `a` to `b`, finishing work on `a` loads `b`.
#[tokio::test]
async fn the_usual_next_model_is_loaded_after_its_predecessor() {
    let mock = Mock::default();
    let (upstream, server) = ollama(mock.clone()).await;
    let router = app(&config(upstream, false)).unwrap();
    for model in [
        "a:latest", "b:latest", "a:latest", "b:latest", "a:latest", "b:latest",
    ] {
        let (status, _, body) = call(
            &router,
            post_json("/_freellama/v1/tasks", None, &task(model)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let before = mock.chats.lock().await.len();
    let (status, _, _) = call(
        &router,
        post_json("/_freellama/v1/tasks", None, &task("a:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let chats = mock.chats.clone();
    assert!(
        wait_for(async || {
            chats.lock().await[before..]
                .iter()
                .any(|chat| chat["model"] == "b:latest" && chat["messages"] == json!([]))
        })
        .await,
        "b was not preloaded: {:?}",
        mock.chats.lock().await[before..].to_vec()
    );
    let (_, _, status_view) = call(
        &router,
        Request::get("/_freellama/v1/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let recent = &status_view["residency"]["audit"]["recent"];
    assert!(
        recent.as_array().is_some_and(|events| events
            .iter()
            .any(|event| event["event"] == "preload"
                && event["trigger"] == "predicted_after:a:latest")),
        "{recent}"
    );
    server.abort();
}

/// With prediction off, nothing loads on a guess.
#[tokio::test]
async fn prediction_can_be_turned_off() {
    let mock = Mock::default();
    let (upstream, server) = ollama(mock.clone()).await;
    let file = runtime_file("[preload]\npredictive = false\n");
    let router = app(&config(upstream, false).with_runtime_config(file.path())).unwrap();
    for model in [
        "a:latest", "b:latest", "a:latest", "b:latest", "a:latest", "b:latest", "a:latest",
    ] {
        call(
            &router,
            post_json("/_freellama/v1/tasks", None, &task(model)),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let warms = mock
        .chats
        .lock()
        .await
        .iter()
        .filter(|chat| chat["messages"] == json!([]))
        .count();
    assert_eq!(warms, 0);
    server.abort();
}

fn key(name: &str, fill: char, daily_tokens: Option<u64>) -> ClientKey {
    ClientKey {
        name: name.into(),
        token: fill.to_string().repeat(40),
        daily_tokens,
        admin: false,
    }
}

/// Each key is metered by the tokens the backend processed for it (prompt minus cache reuse,
/// plus output); a spent budget refuses generation until the next UTC day, never reads.
#[tokio::test]
async fn client_keys_are_metered_budgeted_and_limited_to_their_rights() {
    let mock = Mock::default();
    let (upstream, server) = ollama(mock.clone()).await;
    let operator = "o".repeat(40);
    let router = app(&config(upstream, false)
        .with_auth_token(operator.clone())
        .with_client_keys(vec![key("agent", 'g', Some(10)), key("tool", 't', None)]))
    .unwrap();
    let agent = "g".repeat(40);
    let (status, _, _) = call(
        &router,
        post_json(
            "/_freellama/v1/tasks",
            Some("wrong-token-wrong-token-wrong-tokens"),
            &task("a:latest"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // 8 prompt tokens, 2 of them reused from the cache, and 5 output tokens: 11 processed.
    let (status, _, body) = call(
        &router,
        post_json("/_freellama/v1/tasks", Some(&agent), &task("a:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = call(
        &router,
        post_json("/_freellama/v1/tasks", Some(&agent), &task("a:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "client_budget_exhausted");
    assert_eq!(body["used_tokens"], 11);
    assert!(headers.get("retry-after").is_some());
    // Reads still work, and the key sees only its own row.
    let (status, _, usage) = call(
        &router,
        Request::get("/_freellama/v1/usage")
            .header("authorization", format!("Bearer {agent}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        usage["clients"]["keys"]["agent"]["tokens_today"], 11,
        "{usage}"
    );
    assert_eq!(usage["clients"]["keys"]["agent"]["remaining_today"], 0);
    assert!(usage["clients"]["keys"].get("tool").is_none());
    // The operator token is unmetered and sees every key.
    let (status, _, _) = call(
        &router,
        post_json("/_freellama/v1/tasks", Some(&operator), &task("a:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A key without admin rights may not move model files.
    let tool = "t".repeat(40);
    let (status, _, body) = call(
        &router,
        post_json("/api/pull", Some(&tool), &json!({"model": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "client_not_admin");
    // Raw generations count too: the proxy reads the counts on their way through.
    let raw = json!({"model": "a:latest", "messages": [{"role": "user", "content": "hi"}], "stream": false});
    let (status, _, _) = call(&router, post_json("/api/chat", Some(&tool), &raw)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, usage) = call(
        &router,
        Request::get("/_freellama/v1/usage")
            .header("authorization", format!("Bearer {operator}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        usage["clients"]["keys"]["tool"]["tokens_today"], 11,
        "{usage}"
    );
    assert_eq!(usage["totals"]["cached_prompt_tokens"], 6, "{usage}");
    server.abort();
}

/// Request bodies an engine received, with the `Authorization` header each carried.
type Seen = Vec<(Value, Option<String>)>;

#[derive(Clone, Default)]
struct Engine {
    seen: Arc<Mutex<Seen>>,
}

async fn engine(engine: Engine) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({"object": "list", "data": [{"id": "qwen-lm"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(engine): State<Engine>, headers: HeaderMap, Json(body): Json<Value>| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                engine.seen.lock().await.push((body.clone(), auth));
                Json(json!({"served_by": "engine", "model": body["model"],
                    "usage": {"prompt_tokens": 4, "completion_tokens": 3}}))
            }),
        )
        .with_state(engine);
    serve(router).await
}

/// `/v1/...` requests go to the engine that serves their model; the caller's bearer token never
/// reaches it, and the merged model list names each engine's models.
#[tokio::test]
async fn openai_requests_route_to_the_engine_that_serves_their_model() {
    let (upstream, ollama_server) = ollama(Mock::default()).await;
    let lm = Engine::default();
    let (lm_url, lm_server) = engine(lm.clone()).await;
    let operator = "o".repeat(40);
    let router = app(&config(upstream, false)
        .with_auth_token(operator.clone())
        .with_upstreams(vec![OpenAiUpstream {
            name: "lm".into(),
            base_url: format!("{lm_url}/v1"),
            api_key: Some("engine-key".into()),
        }]))
    .unwrap();
    let chat = |model: &str| {
        post_json(
            "/v1/chat/completions",
            Some(&operator),
            &json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
        )
    };
    let (status, headers, body) = call(&router, chat("lm/qwen-lm")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["served_by"], "engine");
    assert_eq!(body["model"], "qwen-lm");
    assert_eq!(headers["x-freellama-upstream"], "lm");
    let (_, _, body) = call(&router, chat("qwen-lm")).await;
    assert_eq!(body["served_by"], "engine", "a bare name the engine lists");
    let (_, _, body) = call(&router, chat("a:latest")).await;
    assert_eq!(body["served_by"], "ollama", "Ollama keeps its own models");
    for (_, auth) in lm.seen.lock().await.iter() {
        assert_eq!(
            auth.as_deref(),
            Some("Bearer engine-key"),
            "only the engine's own key"
        );
    }
    let (_, _, models) = call(
        &router,
        Request::get("/v1/models")
            .header("authorization", format!("Bearer {operator}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert_eq!(ids, ["a:latest", "lm/qwen-lm"], "{models}");
    let (_, _, usage) = call(
        &router,
        Request::get("/_freellama/v1/usage")
            .header("authorization", format!("Bearer {operator}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        usage["by_model"]["lm/qwen-lm"]["prompt_tokens"], 4,
        "{usage}"
    );
    ollama_server.abort();
    lm_server.abort();
}

/// Under thermal throttling a cold load waits and is refused with a retry hint, while a model
/// Ollama already holds keeps answering: waiting would cool nothing it heats.
#[tokio::test]
async fn a_thermal_hold_stops_cold_loads_but_not_resident_runners() {
    let mock = Mock::default();
    mock.loaded.lock().await.push(json!({
        "name": "a:latest", "digest": "d-a", "size": 1000, "size_vram": 1000,
        "context_length": 8192, "expires_at": "2026-01-01T00:05:00Z"
    }));
    let (upstream, server) = ollama(mock.clone()).await;
    let router = app(&config(upstream, true)).unwrap();
    let (status, _, body) = call(
        &router,
        post_json("/_freellama/v1/tasks", None, &task("a:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = call(
        &router,
        post_json("/_freellama/v1/tasks", None, &task("b:latest")),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(headers.get("retry-after").is_some(), "{body}");
    assert_eq!(
        body["resource_admission"]["held_by"],
        json!(["thermal_throttling"]),
        "{body}"
    );
    server.abort();
}
