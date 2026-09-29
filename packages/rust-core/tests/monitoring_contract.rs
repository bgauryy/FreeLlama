//! Monitoring endpoints, runtime config reload, the upstream circuit breaker, and leaving
//! `num_ctx` to Ollama when its default for the model is known.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::app;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

mod common;

type Captured = Arc<Mutex<Vec<Value>>>;

/// A one-model upstream whose Modelfile sets `num_ctx` (as `/api/show` reports it).
async fn upstream(
    modelfile_parameters: &'static str,
    chat_status: StatusCode,
) -> (String, Captured) {
    let captured = Captured::default();
    let seen = Arc::clone(&captured);
    let router = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models":[{"name":"chat:latest","size":1,"digest":"d1"}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route(
            "/api/show",
            post(move || async move {
                Json(json!({
                    "capabilities": ["completion"],
                    "model_info": {"general.architecture": "llama", "llama.context_length": 32768},
                    "parameters": modelfile_parameters,
                }))
            }),
        )
        .route(
            "/api/chat",
            post(move |Json(body): Json<Value>| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().await.push(body);
                    (
                        chat_status,
                        Json(json!({
                            "message": {"role": "assistant", "content": "ok"},
                            "done": true,
                            "prompt_eval_count": 12,
                            "eval_count": 30,
                            "eval_duration": 1_000_000_000_u64,
                        })),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), captured)
}

fn task() -> Request<Body> {
    Request::post("/_freellama/v1/tasks")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"task":"completion","objective":"fastest","model":"chat:latest","prompt":"hello"}).to_string(),
        ))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn get_json(platform: &Router, path: &str) -> Value {
    let response = platform
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    body_json(response).await
}

#[tokio::test]
async fn modelfile_context_is_left_to_ollama_and_explicit_mode_still_sends_it() {
    let (address, captured) = upstream("num_ctx 16384", StatusCode::OK).await;
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        &address,
        None,
        None,
        "chat:latest",
    ))
    .unwrap();
    let response = platform.clone().oneshot(task()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let receipt = body_json(response).await;
    let sent = captured.lock().await.pop().unwrap();
    assert!(
        sent["options"].get("num_ctx").is_none(),
        "num_ctx must be left to Ollama: {sent}"
    );
    assert_eq!(
        receipt["execution"]["context_sizing"]["num_ctx_sent"],
        false
    );
    assert_eq!(
        receipt["execution"]["context_sizing"]["ollama_default_context"]["source"],
        "modelfile"
    );
    assert_eq!(
        receipt["route"]["options"]["num_ctx"], 16384,
        "estimates use what Ollama will allocate"
    );

    // The same request in explicit mode keeps sending the sized context.
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runtime.toml");
    std::fs::write(&file, "context_mode = \"explicit\"\n").unwrap();
    let explicit =
        app(
            &common::platform_config("127.0.0.1:11435", &address, None, None, "chat:latest")
                .with_runtime_config(&file),
        )
        .unwrap();
    assert_eq!(
        explicit.oneshot(task()).await.unwrap().status(),
        StatusCode::OK
    );
    let sent = captured.lock().await.pop().unwrap();
    assert_eq!(sent["options"]["num_ctx"], 2048);
}

#[tokio::test]
async fn a_modelfile_context_too_small_for_the_request_is_not_trusted() {
    let (address, captured) = upstream("num_ctx 1024", StatusCode::OK).await;
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        &address,
        None,
        None,
        "chat:latest",
    ))
    .unwrap();
    assert_eq!(
        platform.oneshot(task()).await.unwrap().status(),
        StatusCode::OK
    );
    let sent = captured.lock().await.pop().unwrap();
    assert_eq!(sent["options"]["num_ctx"], 2048);
}

#[tokio::test]
async fn metrics_status_usage_and_config_report_finished_work() {
    let (address, _) = upstream("", StatusCode::OK).await;
    let directory = tempfile::tempdir().unwrap();
    let ledger = directory.path().join("usage.jsonl");
    let platform =
        app(
            &common::platform_config("127.0.0.1:11435", &address, None, None, "chat:latest")
                .with_usage_file(&ledger),
        )
        .unwrap();
    assert_eq!(
        platform.clone().oneshot(task()).await.unwrap().status(),
        StatusCode::OK
    );

    let metrics = platform
        .clone()
        .oneshot(
            Request::get("/_freellama/v1/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
    assert!(
        metrics.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = String::from_utf8(
        to_bytes(metrics.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for needle in [
        "freellama_tasks_total{model=\"chat:latest\",backend=\"gpu\",task=\"completion\",outcome=\"ok\"} 1",
        "freellama_output_tokens_total{model=\"chat:latest\",backend=\"gpu\",task=\"completion\",outcome=\"ok\"} 30",
        "# TYPE freellama_queue_depth gauge",
        "freellama_admission_limit_units{backend=\"gpu\"} 2",
        "freellama_ollama_num_parallel",
        "freellama_host_memory_available_bytes",
    ] {
        assert!(text.contains(needle), "missing {needle} in\n{text}");
    }

    let status = get_json(&platform, "/_freellama/v1/status").await;
    assert_eq!(status["backends"]["gpu"]["circuit"]["state"], "closed");
    assert_eq!(status["usage_today"]["tasks"], 1);
    assert!(status["loaded_models"].is_array());
    assert!(status["ollama"]["config"]["settings"]["OLLAMA_NUM_PARALLEL"]["source"].is_string());

    let usage = get_json(&platform, "/_freellama/v1/usage?days=7").await;
    assert_eq!(usage["by_model"]["chat:latest"]["output_tokens"], 30);
    assert_eq!(usage["ledger"]["records"], 1);
    let line = std::fs::read_to_string(&ledger).unwrap();
    assert_eq!(line.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(line.trim()).unwrap()["model"],
        "chat:latest"
    );

    let config = get_json(&platform, "/_freellama/v1/config").await;
    assert_eq!(config["settings"]["max_concurrent_tasks"]["value"], 2);
    assert_eq!(
        config["settings"]["max_concurrent_tasks"]["source"],
        "ollama_num_parallel"
    );
}

#[tokio::test]
async fn a_config_reload_changes_admission_without_a_restart() {
    let (address, _) = upstream("", StatusCode::OK).await;
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runtime.toml");
    std::fs::write(&file, "max_concurrent_tasks = 3\nmax_queued_tasks = 4\n").unwrap();
    let platform =
        app(
            &common::platform_config("127.0.0.1:11435", &address, None, None, "chat:latest")
                .with_runtime_config(&file),
        )
        .unwrap();
    let health = get_json(&platform, "/_freellama/v1/health").await;
    assert_eq!(health["backends"]["gpu"]["admission"]["slots_total"], 3);

    std::fs::write(&file, "max_concurrent_tasks = 6\nmax_queued_tasks = 9\n").unwrap();
    let reloaded = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/config/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reloaded.status(), StatusCode::OK);
    assert_eq!(body_json(reloaded).await["changed"], true);
    let admission =
        get_json(&platform, "/_freellama/v1/health").await["backends"]["gpu"]["admission"].clone();
    assert_eq!(admission["slots_total"], 6);
    assert_eq!(admission["queue_limit"], 9);

    std::fs::write(&file, "max_concurrent_tasks = \"lots\"\n").unwrap();
    let rejected = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/config/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let admission =
        get_json(&platform, "/_freellama/v1/health").await["backends"]["gpu"]["admission"].clone();
    assert_eq!(
        admission["slots_total"], 6,
        "a bad file keeps the last good values"
    );
}

#[tokio::test]
async fn repeated_upstream_failures_open_the_circuit_and_fail_fast() {
    let (address, captured) = upstream("", StatusCode::INTERNAL_SERVER_ERROR).await;
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runtime.toml");
    std::fs::write(
        &file,
        "breaker_failures = 2\nbreaker_cooldown_seconds = 30\n",
    )
    .unwrap();
    let platform =
        app(
            &common::platform_config("127.0.0.1:11435", &address, None, None, "chat:latest")
                .with_runtime_config(&file),
        )
        .unwrap();
    for _ in 0..2 {
        let response = platform.clone().oneshot(task()).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    let reached = captured.lock().await.len();
    let refused = platform.clone().oneshot(task()).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(refused.headers().contains_key("retry-after"));
    assert_eq!(body_json(refused).await["code"], "upstream_circuit_open");
    assert_eq!(
        captured.lock().await.len(),
        reached,
        "an open circuit does not reach Ollama"
    );
    let status = get_json(&platform, "/_freellama/v1/status").await;
    assert_eq!(status["backends"]["gpu"]["circuit"]["state"], "open");
}

#[test]
fn the_example_runtime_file_parses_and_every_key_is_documented() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../freellama.runtime.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    freellama::platform::RuntimeFile::load(&path).unwrap();
    // Uncommenting every example line must still be a valid file.
    let uncommented = text
        .lines()
        .filter_map(|line| line.strip_prefix("# ").filter(|rest| rest.contains(" = ")))
        .collect::<Vec<_>>()
        .join("\n");
    let file: freellama::platform::RuntimeFile = toml::from_str(&uncommented).unwrap();
    let keys = serde_json::to_value(&file).unwrap();
    for (key, value) in keys.as_object().unwrap() {
        assert!(
            !value.is_null(),
            "{key} is missing from freellama.runtime.example.toml"
        );
    }
}
