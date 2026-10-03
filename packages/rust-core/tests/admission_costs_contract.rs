use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{RuntimeFile, app};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tower::ServiceExt;

mod common;

const MODEL: &str = "costs:latest";

struct Fixture {
    platform: Router,
    directory: tempfile::TempDir,
    blocking: Arc<AtomicBool>,
    release: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(runtime: &str, pinned_units: Option<usize>) -> Fixture {
    let blocking = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let chat_blocking = Arc::clone(&blocking);
    let chat_release = Arc::clone(&release);
    let upstream = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[{"name":MODEL,"digest":"revision-a","size":1000}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[{"name":MODEL,"digest":"revision-a","size":1000,"size_vram":1000,"context_length":4096}]})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion","vision","embedding"],"model_info":{"general.architecture":"llama","llama.context_length":8192}})) }))
        .route("/api/chat", post(move || {
            let blocking = Arc::clone(&chat_blocking);
            let release = Arc::clone(&chat_release);
            async move {
                let released = release.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                if blocking.load(Ordering::SeqCst) {
                    released.await;
                }
                Json(json!({"done":true,"message":{"role":"assistant","content":"ok"}}))
            }
        }))
        .route("/api/embed", post(|Json(body): Json<Value>| async move {
            let items = body["input"].as_array().map_or(1, Vec::len);
            Json(json!({"embeddings":vec![vec![0.1,0.2];items]}))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runtime.toml");
    std::fs::write(&file, runtime).unwrap();
    let mut config = common::platform_config("127.0.0.1:11435", endpoint, None, None, MODEL)
        .with_runtime_config(file);
    if let Some(units) = pinned_units {
        config = config.with_max_concurrent_tasks(units);
    }
    Fixture {
        platform: app(&config).unwrap(),
        directory,
        blocking,
        release,
        server,
    }
}

#[tokio::test]
async fn queued_work_keeps_its_cost_while_reload_changes_new_work() {
    let fixture = Arc::new(fixture("[task_costs]\nvision = 2\n", Some(8)).await);
    fixture.blocking.store(true, Ordering::SeqCst);
    let payload = json!({"task":"vision","model":MODEL,"objective":"fastest","context_tokens":4096,"prompt":"fixture"});
    let mut holders = Vec::new();
    for _ in 0..4 {
        let fixture = Arc::clone(&fixture);
        let payload = payload.clone();
        holders.push(tokio::spawn(async move {
            request(&fixture, "POST", "tasks", payload).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, health) = request(&fixture, "GET", "health", Value::Null).await;
            if health["backends"]["gpu"]["admission"]["active_units"] == 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("holders must occupy all eight units");
    let queued_fixture = Arc::clone(&fixture);
    let queued =
        tokio::spawn(async move { request(&queued_fixture, "POST", "tasks", payload).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, health) = request(&fixture, "GET", "health", Value::Null).await;
            if health["backends"]["gpu"]["admission"]["queue_depth"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the old-cost request must be registered before reload");
    std::fs::write(
        fixture.directory.path().join("runtime.toml"),
        "[task_costs]\nvision = 3\n",
    )
    .unwrap();
    let (status, result) = request(&fixture, "POST", "config/reload", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, preview) = request(
        &fixture,
        "POST",
        "routes",
        json!({"task":"vision","model":MODEL,"context_tokens":4096}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["execution"]["agent_plan"]["task_cost_units"], 3);
    fixture.blocking.store(false, Ordering::SeqCst);
    fixture.release.notify_waiters();
    for holder in holders {
        let (status, result) = holder.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["admission"]["cost"], 2);
    }
    let (status, result) = queued.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["agent_plan"]["task_cost_units"], 2);
    assert_eq!(result["admission"]["cost"], 2);
    assert_cost(&fixture, "vision", 3, None).await;
}

async fn request(fixture: &Fixture, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = fixture
        .platform
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/_freellama/v1/{path}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn assert_cost(fixture: &Fixture, task: &str, expected: u64, input: Option<Value>) {
    let route = json!({"task":task,"model":MODEL,"objective":"fastest","context_tokens":4096});
    let (status, preview) = request(fixture, "POST", "routes", route.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let scalar = input.as_ref().is_none_or(|value| !value.is_array());
    if scalar {
        assert_eq!(
            preview["execution"]["agent_plan"]["task_cost_units"],
            expected
        );
    }
    let mut payload = route;
    if let Some(input) = input {
        payload["input"] = input;
    } else {
        payload["prompt"] = json!("fixture");
    }
    let (status, result) = request(fixture, "POST", "tasks", payload).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result["execution"]["agent_plan"]["task_cost_units"],
        expected
    );
    assert_eq!(result["admission"]["cost"], expected);
    let (_, health) = request(fixture, "GET", "health", Value::Null).await;
    assert_eq!(health["backends"]["gpu"]["admission"]["active_units"], 0);
}

#[tokio::test]
async fn partial_cost_overrides_agree_in_preview_execution_and_config() {
    let fixture = fixture("[task_costs]\nvision = 2\ncoding = 7\n", Some(8)).await;
    assert_cost(&fixture, "vision", 2, None).await;
    assert_cost(&fixture, "completion", 2, None).await;
    assert_cost(&fixture, "coding", 7, None).await;
    assert_cost(&fixture, "embedding", 1, Some(json!("one"))).await;
    let (_, config) = request(&fixture, "GET", "config", Value::Null).await;
    assert_eq!(config["settings"]["task_costs"]["value"]["vision"], 2);
    assert_eq!(config["settings"]["task_costs"]["value"]["completion"], 2);
    assert_eq!(config["settings"]["task_costs"]["source"], "file");
    let (_, health) = request(&fixture, "GET", "health", Value::Null).await;
    assert_eq!(health["admission"]["costs"]["vision"], 2);
    assert_eq!(health["admission"]["costs"]["chat"], 2);
    assert_eq!(health["admission"]["costs"]["base_by_task"]["coding"], 7);
    assert_eq!(config["settings"]["task_costs"]["value"]["coding"], 7);
}

#[tokio::test]
async fn defaults_and_embedding_cardinality_are_preserved() {
    let fixture = fixture("", Some(8)).await;
    assert_cost(&fixture, "vision", 4, None).await;
    assert_cost(&fixture, "completion", 2, None).await;
    assert_cost(&fixture, "embedding", 3, Some(json!(vec!["item"; 9]))).await;
}

async fn queued_charge_after_resize(
    initial: usize,
    holder_cost: u32,
    resized: usize,
    expected: u64,
) {
    let runtime = format!(
        "max_concurrent_tasks = {initial}\n[task_costs]\nvision = 4\ncompletion = {holder_cost}\n"
    );
    let fixture = Arc::new(fixture(&runtime, None).await);
    fixture.blocking.store(true, Ordering::SeqCst);
    let holder_fixture = Arc::clone(&fixture);
    let holder = tokio::spawn(async move {
        request(
            &holder_fixture,
            "POST",
            "tasks",
            json!({"task":"completion","model":MODEL,"context_tokens":4096,"prompt":"holder"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, health) = request(&fixture, "GET", "health", Value::Null).await;
            if health["backends"]["gpu"]["admission"]["active_units"] == holder_cost {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let queued_fixture = Arc::clone(&fixture);
    let queued = tokio::spawn(async move {
        request(
            &queued_fixture,
            "POST",
            "tasks",
            json!({"task":"vision","model":MODEL,"context_tokens":4096,"prompt":"queued"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, health) = request(&fixture, "GET", "health", Value::Null).await;
            if health["backends"]["gpu"]["admission"]["queue_depth"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::write(fixture.directory.path().join("runtime.toml"), format!("max_concurrent_tasks = {resized}\n[task_costs]\nvision = 4\ncompletion = {holder_cost}\n")).unwrap();
    let (status, result) = request(&fixture, "POST", "config/reload", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    fixture.blocking.store(false, Ordering::SeqCst);
    fixture.release.notify_waiters();
    assert_eq!(holder.await.unwrap().0, StatusCode::OK);
    let (status, result) = queued.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result["admission"]["cost"], expected,
        "charge must use capacity at acquisition"
    );
    let active = result["admission"]["slots_total"].as_u64().unwrap()
        - result["admission"]["slots_available_during_call"]
            .as_u64()
            .unwrap();
    assert_eq!(
        active, expected,
        "the receipt must equal the permit's actual units"
    );
}

#[tokio::test]
async fn queued_cost_receipt_tracks_a_limit_reduction() {
    queued_charge_after_resize(4, 2, 2, 2).await;
}

#[tokio::test]
async fn queued_work_retains_its_raw_cost_when_capacity_grows() {
    queued_charge_after_resize(2, 1, 4, 4).await;
}

#[tokio::test]
async fn embedding_overrides_scale_each_group_of_four_items() {
    let fixture = fixture("[task_costs]\nembedding = 2\n", Some(8)).await;
    assert_cost(&fixture, "embedding", 2, Some(json!("one"))).await;
    assert_cost(&fixture, "embedding", 6, Some(json!(vec!["item"; 9]))).await;
}

#[tokio::test]
async fn the_largest_positive_cost_still_fits_the_backend_limit() {
    let fixture = fixture("[task_costs]\nvision = 4294967295\n", Some(8)).await;
    assert_cost(&fixture, "vision", 8, None).await;
    let (_, config) = request(&fixture, "GET", "config", Value::Null).await;
    assert_eq!(
        config["settings"]["task_costs"]["value"]["vision"],
        u32::MAX
    );
}

#[tokio::test]
async fn reload_applies_valid_costs_and_rejects_invalid_maps_atomically() {
    let fixture = fixture("[task_costs]\nvision = 2\n", Some(8)).await;
    let file = fixture.directory.path().join("runtime.toml");
    std::fs::write(&file, "[task_costs]\nvision = 3\n").unwrap();
    let (status, result) = request(&fixture, "POST", "config/reload", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_cost(&fixture, "vision", 3, None).await;
    for invalid in [
        "[task_costs]\nvision = 1\nvison = 2\n",
        "[task_costs]\nvision = 1\ncompletion = 0\n",
        "[task_costs]\nvision = 1\ncompletion = 4294967296\n",
    ] {
        std::fs::write(&file, invalid).unwrap();
        let (status, result) = request(&fixture, "POST", "config/reload", Value::Null).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{result}");
        assert_cost(&fixture, "vision", 3, None).await;
        let (_, config) = request(&fixture, "GET", "config", Value::Null).await;
        assert_eq!(config["settings"]["task_costs"]["value"]["completion"], 2);
    }
}

#[test]
fn invalid_task_names_zero_and_overflow_fail_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runtime.toml");
    for (text, expected) in [
        ("[task_costs]\nvison = 2\n", "unknown variant"),
        ("[task_costs]\nvision = 0\n", "nonzero"),
        ("[task_costs]\nvision = 4294967296\n", "u32"),
    ] {
        std::fs::write(&file, text).unwrap();
        let error = RuntimeFile::load(&file).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}
