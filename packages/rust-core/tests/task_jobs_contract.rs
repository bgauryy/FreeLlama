use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

async fn fixture() -> (
    Router,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
    tokio::task::JoinHandle<()>,
) {
    fixture_with_delay(Duration::ZERO).await
}

async fn fixture_with_delay(
    delay: Duration,
) -> (
    Router,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
    tokio::task::JoinHandle<()>,
) {
    let available = Arc::new(AtomicU64::new(50));
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let upstream = Router::new()
        .route("/api/tags", get(|| async { Json(json!({"models":[{"name":"helper:latest","digest":"d","size":1000}]})) }))
        .route("/api/ps", get(|| async { Json(json!({"models":[]})) }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":32768}})) }))
        .route("/api/chat", post(move || { let count=count.clone(); async move { count.fetch_add(1, Ordering::SeqCst); tokio::time::sleep(delay).await; Json(json!({"done":true,"message":{"content":"ok"}})) } }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let memory = available.clone();
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            hold_available_percent: 1,
            resume_available_percent: 2,
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "test".into(),
            total_memory_bytes: Some(10000),
            available_memory_bytes: Some(memory.load(Ordering::SeqCst)),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            ..HostResources::default()
        },
    )
    .unwrap();
    let mut config = PlatformConfig::new("127.0.0.1:11435", endpoint, None, None, "helper:latest")
        .with_max_queue_wait(Duration::from_secs(5));
    config.resource_governor = governor;
    (app(&config).unwrap(), available, calls, server)
}

async fn request(platform: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = platform
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn wait_status(platform: &Router, id: &str, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, body) = request(platform, "GET", &format!("jobs/{id}"), Value::Null).await;
            if body["job"]["status"] == expected {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("job must reach expected state")
}

fn task() -> Value {
    json!({"task":"completion","objective":"fastest","model":"helper:latest","prompt":"private prompt","defer":true,"timeout_seconds":10})
}

#[tokio::test]
#[ignore = "fixed-budget control timing experiment; no model inference"]
async fn measure_deferred_control_latency() {
    let (platform, memory, calls, server) = fixture().await;
    let mut samples = std::collections::BTreeMap::<&str, Vec<f64>>::new();
    for _ in 0..20 {
        let started = std::time::Instant::now();
        let (status, accepted) = request(&platform, "POST", "tasks", task()).await;
        samples
            .entry("submit")
            .or_default()
            .push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = accepted["job"]["id"].as_str().unwrap();
        wait_status(&platform, id, "waiting_for_resources").await;
        for (name, path) in [("get", format!("jobs/{id}")), ("list", "jobs".into())] {
            let started = std::time::Instant::now();
            let (status, _) = request(&platform, "GET", &path, Value::Null).await;
            samples
                .entry(name)
                .or_default()
                .push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(status, StatusCode::OK);
        }
        let started = std::time::Instant::now();
        let (status, cancelled) =
            request(&platform, "POST", &format!("jobs/{id}/cancel"), Value::Null).await;
        samples
            .entry("cancel")
            .or_default()
            .push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cancelled["job"]["status"], "cancelled");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for _ in 0..20 {
        memory.store(50, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let (_, accepted) = request(&platform, "POST", "tasks", task()).await;
        let id = accepted["job"]["id"].as_str().unwrap();
        wait_status(&platform, id, "waiting_for_resources").await;
        let started = std::time::Instant::now();
        memory.store(9000, Ordering::SeqCst);
        wait_status(&platform, id, "completed").await;
        samples
            .entry("resource_recovery")
            .or_default()
            .push(started.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 20);
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
    assert_eq!(health["backends"]["gpu"]["admission"]["queue_depth"], 0);
    for (name, mut samples) in samples {
        samples.sort_by(f64::total_cmp);
        println!(
            "CONTROL_TIMING {}",
            json!({"case":name,"trials":samples.len(),"p50_ms":samples[9],"p95_ms":samples[18],"samples_ms":samples})
        );
    }
    server.abort();
}

#[tokio::test]
async fn deferred_job_is_inspectable_waits_for_memory_and_completes_after_recovery() {
    let (platform, memory, calls, server) = fixture().await;
    let (status, accepted) = request(&platform, "POST", "tasks", task()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["job"]["id"].as_str().unwrap();
    let waiting = wait_status(&platform, id, "waiting_for_resources").await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!waiting.to_string().contains("private prompt"));
    assert_eq!(waiting["job"]["selected_model"], "helper:latest");
    let (_, listed) = request(&platform, "GET", "jobs", Value::Null).await;
    assert_eq!(listed["jobs"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains("private prompt"));
    memory.store(9000, Ordering::SeqCst);
    let completed = wait_status(&platform, id, "completed").await;
    assert_eq!(
        completed["job"]["result"]["response"]["message"]["content"],
        "ok"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn cancel_one_job_reclaims_capacity_and_leaves_other_work_live() {
    let (platform, memory, calls, server) = fixture().await;
    let (_, first) = request(&platform, "POST", "tasks", task()).await;
    let first = first["job"]["id"].as_str().unwrap();
    wait_status(&platform, first, "waiting_for_resources").await;
    let (_, second) = request(&platform, "POST", "tasks", task()).await;
    let second = second["job"]["id"].as_str().unwrap();
    let (status, cancelled) = request(
        &platform,
        "POST",
        &format!("jobs/{first}/cancel"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["job"]["status"], "cancelled");
    let (status, again) = request(
        &platform,
        "POST",
        &format!("jobs/{first}/cancel"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["job"]["status"], "cancelled");
    memory.store(9000, Ordering::SeqCst);
    wait_status(&platform, second, "completed").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
    assert_eq!(health["backends"]["gpu"]["admission"]["queue_depth"], 0);
    server.abort();
}

#[tokio::test]
async fn removing_a_running_job_cancels_it_releases_capacity_and_preserves_other_jobs() {
    let (platform, memory, calls, server) = fixture_with_delay(Duration::from_secs(2)).await;
    memory.store(9000, Ordering::SeqCst);
    let (_, first) = request(&platform, "POST", "tasks", task()).await;
    let first = first["job"]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (_, second) = request(&platform, "POST", "tasks", task()).await;
    let second = second["job"]["id"].as_str().unwrap();
    let (status, removed) =
        request(&platform, "DELETE", &format!("jobs/{first}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["id"], first);
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["status"], "cancelled");
    assert!(removed.get("result").is_none());
    assert_eq!(
        request(&platform, "GET", &format!("jobs/{first}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (_, listed) = request(&platform, "GET", "jobs", Value::Null).await;
    assert_eq!(listed["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(listed["jobs"][0]["id"], second);
    wait_status(&platform, second, "completed").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
    assert_eq!(health["backends"]["gpu"]["admission"]["active_units"], 0);
    assert_eq!(
        request(&platform, "DELETE", &format!("jobs/{first}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    server.abort();
}

#[tokio::test]
async fn removing_a_completed_job_discards_its_result_without_new_inference() {
    let (platform, memory, calls, server) = fixture().await;
    memory.store(9000, Ordering::SeqCst);
    let (_, accepted) = request(&platform, "POST", "tasks", task()).await;
    let id = accepted["job"]["id"].as_str().unwrap();
    wait_status(&platform, id, "completed").await;
    let (status, removed) = request(&platform, "DELETE", &format!("jobs/{id}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(
        removed,
        json!({"id":id,"removed":true,"status":"completed","scope":"process_memory"})
    );
    let (_, listed) = request(&platform, "GET", "jobs", Value::Null).await;
    assert!(listed["jobs"].as_array().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn total_deadline_expires_held_work_without_inference() {
    let (platform, _, calls, server) = fixture().await;
    let mut body = task();
    body["timeout_seconds"] = json!(1);
    let (status, accepted) = request(&platform, "POST", "tasks", body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["job"]["id"].as_str().unwrap();
    let expired = wait_status(&platform, id, "expired").await;
    assert_eq!(expired["job"]["error"]["code"], "task_deadline_exceeded");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["backends"]["gpu"]["admission"]["active_units"], 0);
    server.abort();
}

#[tokio::test]
async fn cancelling_an_inflight_job_releases_local_slots_and_reservations() {
    let (platform, memory, calls, server) = fixture_with_delay(Duration::from_secs(30)).await;
    memory.store(9000, Ordering::SeqCst);
    let (_, accepted) = request(&platform, "POST", "tasks", task()).await;
    let id = accepted["job"]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (_, cancelled) =
        request(&platform, "POST", &format!("jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(cancelled["job"]["status"], "cancelled");
    assert_eq!(cancelled["job"]["error"]["code"], "task_cancelled");
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["backends"]["gpu"]["admission"]["active_units"], 0);
    assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
    server.abort();
}

#[tokio::test]
async fn synchronous_total_deadline_includes_inference_and_releases_permits() {
    let (platform, memory, calls, server) = fixture_with_delay(Duration::from_secs(30)).await;
    memory.store(9000, Ordering::SeqCst);
    let mut body = task();
    body["defer"] = json!(false);
    body["timeout_seconds"] = json!(1);
    let (status, expired) = request(&platform, "POST", "tasks", body).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{expired}");
    assert_eq!(expired["code"], "task_deadline_exceeded");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (_, health) = request(&platform, "GET", "health", Value::Null).await;
    assert_eq!(health["backends"]["gpu"]["admission"]["active_units"], 0);
    assert_eq!(health["admission"]["resources"]["reserved_bytes"], 0);
    server.abort();
}

#[tokio::test]
async fn deferred_inputs_are_bounded_and_cannot_be_nested_in_batches() {
    let (platform, _, calls, server) = fixture().await;
    let mut large = task();
    large["prompt"] = json!("x".repeat(1024 * 1024));
    let (status, refusal) = request(&platform, "POST", "tasks", large).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{refusal}");
    assert_eq!(refusal["code"], "task_job_input_too_large");
    let (_, listed) = request(&platform, "GET", "jobs", Value::Null).await;
    assert!(listed["jobs"].as_array().unwrap().is_empty());
    let (status, refusal) = request(
        &platform,
        "POST",
        "task-batches",
        json!({"tasks":[{"id":"nested","independent":true,"task":task()}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refusal}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}
