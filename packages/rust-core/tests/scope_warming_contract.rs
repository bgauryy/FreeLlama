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

async fn fixture() -> (Router, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let upstream = Router::new()
        .route("/api/tags",get(|| async {Json(json!({"models":[{"name":"test:latest","digest":"d","size":1}]}))}))
        .route("/api/ps",get(|| async {Json(json!({"models":[]}))}))
        .route("/api/show",post(|| async {Json(json!({"capabilities":["completion","tools"],"model_info":{"llama.context_length":32768}}))}))
        .route("/api/chat",post(move |Json(body):Json<Value>| {let seen=seen.clone(); async move {
            let content=body["messages"].as_array().and_then(|messages| messages.last()).and_then(|message| message["content"].as_str()).unwrap_or_default().to_owned();
            seen.lock().await.push(body);
            if content == "block" { tokio::time::sleep(std::time::Duration::from_millis(500)).await; }
            if content == "expire" { tokio::time::sleep(std::time::Duration::from_millis(1200)).await; }
            if content == "fail" { return Json(json!({"done":false,"message":{"role":"assistant","content":"partial"}})); }
            Json(json!({"done":true,"message":{"role":"assistant","content":"ok","thinking":"full reasoning","tool_calls":[{"function":{"name":"lookup","arguments":{"id":1}}}]}}))
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    (
        app(&common::platform_config(
            "127.0.0.1:11435",
            endpoint,
            None,
            None,
            "test:latest",
        ))
        .unwrap(),
        captured,
        server,
    )
}

#[tokio::test]
async fn scope_history_is_opt_in_and_forks_are_isolated_snapshots() {
    let (platform, captured, server) = fixture().await;
    let initial = json!([{"role":"system","content":"  stable prefix\n"}]);
    let (status,created)=request(&platform,"POST","scopes",json!({"messages":initial,"route_defaults":{"task":"coding","objective":"quality","model":"test:latest"}})).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created.get("messages").is_none());
    let id = created["scope_id"].as_str().unwrap();
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"next"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["route"]["task"], "coding");
    assert_eq!(result["scope"]["revision"], 1);
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["messages"][0], initial[0]);
    assert_eq!(history["messages"][2]["thinking"], "full reasoning");
    assert!(history["messages"][2]["tool_calls"].is_array());
    let (status, fork) = request(
        &platform,
        "POST",
        &format!("scopes/{id}/fork"),
        json!({"revision":1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id = fork["scope_id"].as_str().unwrap();
    let (status,_)=request(&platform,"POST","tasks",json!({"scope_id":fork_id,"scope_revision":0,"prompt":"branch","task":"completion","objective":"balanced"})).await;
    assert_eq!(status, StatusCode::OK);
    let (_, source) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(source["messages"].as_array().unwrap().len(), 3);
    let calls = captured.lock().await;
    assert_eq!(calls[0]["messages"][0], initial[0]);
    assert_eq!(calls[1]["messages"][0], initial[0]);
    drop(calls);
    server.abort();
}

#[tokio::test]
async fn failed_and_stale_scope_tasks_never_append() {
    let (platform, captured, server) = fixture().await;
    let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
    let id = created["scope_id"]
        .as_str()
        .expect("scope endpoint creates an id");
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"fail"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{result}");
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["messages"], json!([]));
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":10,"prompt":"stale"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(result["code"], "scope_revision_conflict");
    assert_eq!(captured.lock().await.len(), 1);
    server.abort();
}

#[tokio::test]
async fn managed_warm_is_an_empty_request_with_finite_residency() {
    let (platform, captured, server) = fixture().await;
    let (status, result) = request(
        &platform,
        "POST",
        "warm",
        json!({"model":"test:latest","context_tokens":4096}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["warm"]["requested"], true);
    let calls = captured.lock().await;
    assert_eq!(calls[0]["messages"], json!([]));
    assert_eq!(calls[0]["options"]["num_ctx"], 4096);
    assert!(calls[0]["keep_alive"].as_str().unwrap().ends_with('s'));
    drop(calls);
    let (status, _) = request(
        &platform,
        "POST",
        "warm",
        json!({"model":"test:latest","prompt":"hidden inference"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(captured.lock().await.len(), 1);
    server.abort();
}

async fn wait_calls(captured: &Arc<Mutex<Vec<Value>>>, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while captured.lock().await.len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_append_is_rejected_before_inference_and_delete_invalidates_commit() {
    let (platform, captured, server) = fixture().await;
    let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
    let id = created["scope_id"].as_str().unwrap().to_owned();
    let worker_platform = platform.clone();
    let worker_id = id.clone();
    let running = tokio::spawn(async move {
        request(
            &worker_platform,
            "POST",
            "tasks",
            json!({"scope_id":worker_id,"scope_revision":0,"prompt":"block"}),
        )
        .await
    });
    wait_calls(&captured, 1).await;
    let (status, busy) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"parallel"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{busy}");
    assert_eq!(busy["code"], "scope_busy");
    assert_eq!(captured.lock().await.len(), 1);
    let (status, fork) = request(
        &platform,
        "POST",
        &format!("scopes/{id}/fork"),
        json!({"revision":0}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    assert_eq!(fork["message_count"], 0);
    assert_eq!(
        request(&platform, "DELETE", &format!("scopes/{id}"), Value::Null)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, result) = running.await.unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND, "{result}");
    assert_eq!(result["code"], "scope_not_found");
    server.abort();
}

#[tokio::test]
async fn cancelled_jobs_release_scope_lease_without_appending() {
    let (platform, captured, server) = fixture().await;
    let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
    let id = created["scope_id"].as_str().unwrap();
    let (status, accepted) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"block","defer":true,"timeout_seconds":5}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    wait_calls(&captured, 1).await;
    let job_id = accepted["job"]["id"].as_str().unwrap();
    let (status, cancelled) = request(
        &platform,
        "POST",
        &format!("jobs/{job_id}/cancel"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["job"]["status"], "cancelled");
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["messages"], json!([]));
    let (status, next) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"after cancellation"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["scope"]["revision"], 1);
    server.abort();
}

#[tokio::test]
async fn expiration_and_deadline_never_commit_partial_histories() {
    let (platform, captured, server) = fixture().await;
    let (_, created) = request(
        &platform,
        "POST",
        "scopes",
        json!({"limits":{"ttl_seconds":1}}),
    )
    .await;
    let id = created["scope_id"].as_str().unwrap();
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"expire","timeout_seconds":3}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{result}");
    assert_eq!(result["code"], "scope_not_found");
    let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
    let id = created["scope_id"].as_str().unwrap();
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"expire","timeout_seconds":1}),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{result}");
    assert_eq!(result["code"], "task_deadline_exceeded");
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["messages"], json!([]));
    assert_eq!(captured.lock().await.len(), 2);
    server.abort();
}

#[tokio::test]
async fn bounded_histories_reject_overflow_and_invalid_scope_controls() {
    let (platform, captured, server) = fixture().await;
    for body in [
        json!({"messages":[{"role":"user","content":"x".repeat(200)}],"limits":{"max_bytes":100}}),
        json!({"limits":{"ttl_seconds":0}}),
        json!({"route_defaults":{"task":"embedding"}}),
        json!({"limits":{"unknown":1}}),
    ] {
        let (status, result) = request(&platform, "POST", "scopes", body).await;
        assert!(!status.is_success(), "{result}");
    }
    let (_,created)=request(&platform,"POST","scopes",json!({"messages":[{"role":"system","content":"prefix"}],"limits":{"max_messages":2,"max_bytes":100}})).await;
    let id = created["scope_id"].as_str().unwrap();
    let (status, result) = request(
        &platform,
        "POST",
        "tasks",
        json!({"scope_id":id,"scope_revision":0,"prompt":"x".repeat(100)}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{result}");
    assert_eq!(result["code"], "scope_history_limit_exceeded");
    for body in [
        json!({"scope_id":id,"prompt":"no revision"}),
        json!({"scope_id":id,"scope_revision":0,"task":"embedding","input":"text"}),
        json!({"scope_id":id,"scope_revision":0,"preview":true}),
        json!({"scope_id":id,"scope_revision":0,"prompt":"tiny","context_tokens":10}),
    ] {
        let (status, result) = request(&platform, "POST", "tasks", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{result}");
    }
    assert_eq!(captured.lock().await.len(), 0);
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["message_count"], 1);
    server.abort();
}

#[tokio::test]
async fn warm_defers_in_existing_job_registry_and_explicit_keep_alive_wins() {
    let (platform, captured, server) = fixture().await;
    let (status, accepted) = request(
        &platform,
        "POST",
        "warm",
        json!({"model":"test:latest","keep_alive":"-1","defer":true,"timeout_seconds":5}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["job"]["id"].as_str().unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let (_, job) = request(&platform, "GET", &format!("jobs/{id}"), Value::Null).await;
            if job["job"]["status"] == "completed" {
                break job;
            }
            assert!(job["job"]["status"] != "failed", "{job}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["job"]["result"]["warm"]["requested"], true);
    assert_eq!(
        completed["job"]["result"]["execution"]["keep_alive"]["mode"],
        "explicit"
    );
    assert_eq!(captured.lock().await[0]["keep_alive"], -1);
    server.abort();
}

#[tokio::test]
async fn scope_and_warm_refused_for_capacity_never_infer_or_append() {
    use freellama::platform::{
        PlatformConfig,
        resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
    };
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let upstream=Router::new()
        .route("/api/tags",get(||async{Json(json!({"models":[{"name":"test:latest","digest":"d","size":1}]}))}))
        .route("/api/ps",get(||async{Json(json!({"models":[]}))}))
        .route("/api/show",post(||async{Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":32768}}))}))
        .route("/api/chat",post(move ||{let count=count.clone();async move {count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);Json(json!({"done":true,"message":{"role":"assistant","content":"unexpected"}}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let mut config = PlatformConfig::new("127.0.0.1:11435", endpoint, None, None, "test:latest")
        .with_max_queue_wait(std::time::Duration::from_millis(50));
    config.resource_governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: std::time::Duration::from_millis(2),
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            ..ResourcePolicy::default()
        },
        || HostResources {
            source: "low_capacity_fixture".into(),
            total_memory_bytes: Some(10000),
            available_memory_bytes: Some(50),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            ..HostResources::default()
        },
    )
    .unwrap();
    let platform = app(&config).unwrap();
    let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
    let id = created["scope_id"].as_str().unwrap();
    for (path, body) in [
        (
            "tasks",
            json!({"model":"test:latest","scope_id":id,"scope_revision":0,"prompt":"must wait"}),
        ),
        ("warm", json!({"model":"test:latest"})),
    ] {
        let (status, result) = request(&platform, "POST", path, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
        assert_eq!(result["code"], "resource_admission_unavailable");
    }
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["messages"], json!([]));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn scope_commit_failure_is_counted_as_a_failed_caller_outcome() {
    let (platform, captured, server) = fixture().await;
    let (_, created) = request(
        &platform,
        "POST",
        "scopes",
        json!({"limits":{"max_messages":1}}),
    )
    .await;
    let id = created["scope_id"].as_str().unwrap();
    let (status,result)=request(&platform,"POST","tasks",json!({"scope_id":id,"scope_revision":0,"model":"test:latest","prompt":"valid inference but history cannot retain assistant"})).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{result}");
    assert_eq!(result["code"], "scope_history_limit_exceeded");
    assert_eq!(captured.lock().await.len(), 1);
    let (_, history) = request(
        &platform,
        "GET",
        &format!("scopes/{id}?include_messages=true"),
        Value::Null,
    )
    .await;
    assert_eq!(history["revision"], 0);
    assert_eq!(history["messages"], json!([]));
    let (_, usage) = request(&platform, "GET", "usage", Value::Null).await;
    assert_eq!(usage["totals"]["tasks"], 1, "{usage}");
    assert_eq!(usage["totals"]["errors"], 1, "{usage}");
    server.abort();
}

#[tokio::test]
async fn warm_immediate_unload_reports_final_residency() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let resident = Arc::new(AtomicBool::new(false));
    let ps = resident.clone();
    let load = resident.clone();
    let unload = resident.clone();
    let upstream=Router::new()
        .route("/api/tags",get(||async{Json(json!({"models":[{"name":"test:latest","digest":"d","size":1}]}))}))
        .route("/api/ps",get(move ||{let ps=ps.clone();async move {Json(json!({"models":if ps.load(Ordering::SeqCst) {vec![json!({"name":"test:latest","digest":"d","size":1,"size_vram":1,"context_length":4096})]} else {vec![]}}))}}))
        .route("/api/show",post(||async{Json(json!({"capabilities":["completion"],"model_info":{"llama.context_length":32768}}))}))
        .route("/api/chat",post(move |Json(body):Json<Value>|{let load=load.clone();async move {assert_eq!(body["messages"],json!([]));load.store(true,Ordering::SeqCst);Json(json!({"done":true,"done_reason":"load"}))}}))
        .route("/api/generate",post(move |Json(body):Json<Value>|{let unload=unload.clone();async move {assert_eq!(body["keep_alive"],0);unload.store(false,Ordering::SeqCst);Json(json!({"done":true}))}}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let platform = app(&common::platform_config(
        "127.0.0.1:11435",
        endpoint,
        None,
        None,
        "test:latest",
    ))
    .unwrap();
    let (status, result) = request(
        &platform,
        "POST",
        "warm",
        json!({"model":"test:latest","keep_alive":"0"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["execution"]["observation"]["status"], "verified");
    assert_eq!(result["execution"]["lifecycle"]["status"], "verified");
    assert_eq!(result["warm"]["loaded"], false, "{result}");
    assert_eq!(
        result["warm"]["residency_source"],
        "ollama_api_ps_after_unload"
    );
    assert!(!resident.load(Ordering::SeqCst));
    server.abort();
}

#[tokio::test]
async fn scope_history_is_independent_of_deleted_affinity_but_session_kill_cancels() {
    let (platform, captured, server) = fixture().await;
    for (index, kill) in [false, true].into_iter().enumerate() {
        let (_, created) = request(&platform, "POST", "scopes", json!({})).await;
        let id = created["scope_id"].as_str().unwrap().to_owned();
        let (_, session) = request(&platform, "POST", "sessions", json!({})).await;
        let session_id = session["session_id"].as_str().unwrap();
        let worker_platform = platform.clone();
        let worker_id = id.clone();
        let worker_session = session_id.to_owned();
        let running = tokio::spawn(async move {
            request(&worker_platform,"POST","tasks",json!({"scope_id":worker_id,"scope_revision":0,"session_id":worker_session,"prompt":"block"})).await
        });
        wait_calls(&captured, index + 1).await;
        if kill {
            assert_eq!(
                request(
                    &platform,
                    "POST",
                    &format!("sessions/{session_id}/kill"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::OK
            );
        } else {
            assert_eq!(
                request(
                    &platform,
                    "DELETE",
                    &format!("sessions/{session_id}"),
                    Value::Null
                )
                .await
                .0,
                StatusCode::NO_CONTENT
            );
        }
        let (status, result) = running.await.unwrap();
        let (_, history) = request(
            &platform,
            "GET",
            &format!("scopes/{id}?include_messages=true"),
            Value::Null,
        )
        .await;
        if kill {
            assert_eq!(status, StatusCode::CONFLICT, "{result}");
            assert_eq!(result["code"], "session_killed");
            assert_eq!(history["revision"], 0);
        } else {
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(history["revision"], 1);
        }
    }
    server.abort();
}
