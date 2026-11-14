//! `stop_on_repetition`: a managed chat streams from Ollama and stops as soon as the output
//! loops, keeping everything generated. Unset, it is on exactly when nothing caps the output.
//!
//! The fake Ollama speaks raw HTTP so it can stream NDJSON line by line and see the client close
//! the connection, which is how a stopped stream cancels generation upstream.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use freellama::platform::{
    PlatformConfig, app,
    resources::{HostResources, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tower::ServiceExt;

/// What the fake `/api/chat` streams.
#[derive(Clone, Copy)]
enum Script {
    /// An answer, then a fence repeated up to `LOOP_LINES` times (a glm-ocr style loop).
    FenceLoop,
    /// An answer, then Ollama's own repeat-guard error line.
    RepeatLimit,
    /// A corrupt middle frame followed by a valid terminal frame must still fail.
    MalformedThenDone,
    /// The answer, then Ollama's final frame with its timing.
    Done,
}

const LOOP_LINES: usize = 600;

#[derive(Clone)]
struct FakeOllama {
    script: Script,
    /// Chat request bodies received.
    chats: Arc<Mutex<Vec<Value>>>,
    /// Streamed chat lines written before the client closed the connection.
    lines_sent: Arc<AtomicUsize>,
}

fn healthy() -> HostResources {
    HostResources {
        source: "injected_contract".into(),
        total_memory_bytes: Some(64 << 30),
        available_memory_bytes: Some(48 << 30),
        memory_pressure: Some(MemoryPressure::Normal),
        thermal_throttled: Some(false),
        ..HostResources::default()
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, Value)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let body = serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null);
    Some((path, body))
}

async fn respond_json(stream: &mut TcpStream, value: &Value) {
    let body = value.to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
}

fn chunk(content: &str) -> String {
    format!(
        "{}\n",
        json!({"model": "loop:latest", "created_at": "2026-10-30T00:00:00Z",
            "message": {"role": "assistant", "content": content}, "done": false})
    )
}

/// Stream the script one line every millisecond until the client closes the connection.
async fn stream_chat(stream: &mut TcpStream, fake: &FakeOllama) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let mut lines = vec![chunk("Invoice #48213\n"), chunk("Total: $1,337.42\n")];
    match fake.script {
        Script::FenceLoop => lines.extend((0..LOOP_LINES).map(|_| chunk("```\n"))),
        Script::RepeatLimit => lines.push(format!(
            "{}\n",
            json!({"error": "prediction aborted, token repeat limit reached"})
        )),
        Script::MalformedThenDone => {
            lines.push("not valid JSON\n".to_owned());
            lines.push(format!("{}\n", json!({"done":true})));
        }
        Script::Done => lines.push(format!(
            "{}\n",
            json!({"model": "loop:latest", "message": {"role": "assistant", "content": ""},
                "done": true, "done_reason": "stop", "eval_count": 2, "eval_duration": 20_000_000_u64,
                "prompt_eval_count": 5, "total_duration": 30_000_000_u64})
        )),
    }
    let mut probe = [0u8; 1];
    for line in lines {
        tokio::select! {
            // A read of 0 bytes is the client closing its side: stop like Ollama does.
            read = stream.read(&mut probe) => if matches!(read, Ok(0) | Err(_)) { return; },
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
        if stream.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        fake.lines_sent.fetch_add(1, Ordering::SeqCst);
    }
}

async fn serve(fake: FakeOllama) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let fake = fake.clone();
            tokio::spawn(async move {
                let Some((path, body)) = read_request(&mut stream).await else {
                    return;
                };
                match path.as_str() {
                    "/api/tags" => {
                        respond_json(
                            &mut stream,
                            &json!({"models": [{"name": "loop:latest", "digest": "d-loop", "size": 1000}]}),
                        )
                        .await;
                    }
                    "/api/show" => {
                        respond_json(
                            &mut stream,
                            &json!({"capabilities": ["completion"],
                                "model_info": {"general.architecture": "llama", "llama.context_length": 8192}}),
                        )
                        .await;
                    }
                    "/api/ps" => {
                        let chats = fake.chats.lock().await;
                        let models = chats.last().map(|body| json!({
                            "name":"loop:latest", "digest":"d-loop", "size":1000,
                            "size_vram":1000, "context_length":body["options"]["num_ctx"].as_u64().unwrap_or(8192),
                        })).into_iter().collect::<Vec<_>>();
                        drop(chats);
                        respond_json(&mut stream, &json!({"models":models})).await;
                    }
                    "/api/chat" => {
                        fake.chats.lock().await.push(body.clone());
                        if body["stream"] == true {
                            stream_chat(&mut stream, &fake).await;
                        } else {
                            respond_json(
                                &mut stream,
                                &json!({"model": "loop:latest", "message": {"role": "assistant", "content": "ok"},
                                    "done": true, "done_reason": "stop", "eval_count": 1}),
                            )
                            .await;
                        }
                    }
                    _ => respond_json(&mut stream, &json!({"done": true})).await,
                }
            });
        }
    });
    (url, server)
}

/// Send a task and return the live response, for reading a stream as it arrives.
async fn send(
    fake: &FakeOllama,
    extra: Value,
) -> (axum::response::Response, tokio::task::JoinHandle<()>) {
    let (upstream, server) = serve(fake.clone()).await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", upstream, None, None)
        .with_max_queue_wait(Duration::from_millis(1500));
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), healthy).unwrap();
    let mut task = json!({"task": "completion", "objective": "fastest", "model": "loop:latest",
        "prompt": "read the invoice", "request_options": {"options": {"num_predict": 2048}}});
    for (key, value) in extra.as_object().unwrap() {
        task[key] = value.clone();
    }
    let response = app(&config)
        .unwrap()
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(task.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (response, server)
}

/// Server-sent events as (event, data) pairs.
fn events(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let field = |name: &str| {
                block
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap_or_default()
                    .to_owned()
            };
            (field("event: "), serde_json::from_str(&field("data: ")).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn a_streamed_task_sends_its_route_each_piece_and_then_the_same_receipt() {
    let fake = fake(Script::Done);
    let (response, server) = send(&fake, json!({"stream": true})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    server.abort();
    let events = events(&text);
    let names: Vec<_> = events.iter().map(|(name, _)| name.as_str()).collect();
    let route = names.iter().position(|name| *name == "route").expect("a route event");
    let first_delta = names.iter().position(|name| *name == "delta").expect("deltas");
    assert!(route < first_delta, "{names:?}");
    assert!(names.contains(&"admission"), "{names:?}");
    assert_eq!(names.last(), Some(&"done"), "{names:?}");
    assert_eq!(events[route].1["selected_model"], "loop:latest");
    let streamed: String = events
        .iter()
        .filter(|(name, _)| name == "delta")
        .filter_map(|(_, data)| data["content"].as_str())
        .collect();
    assert_eq!(streamed, "Invoice #48213\nTotal: $1,337.42\n");
    let receipt = &events.last().unwrap().1;
    assert_eq!(receipt["response"]["message"]["content"], streamed);
    assert_eq!(receipt["route"]["selected_model"], "loop:latest");
    assert!(receipt["timing"]["upstream_ms"].is_number(), "{receipt}");
    // Streaming alone does not turn on the loop stop: num_predict caps this answer.
    assert_eq!(receipt["response"]["done_reason"], "stop");
}

#[tokio::test]
async fn a_stream_that_cannot_carry_tokens_is_refused_before_admission() {
    for extra in [
        json!({"stream": true, "defer": true}),
        json!({"stream": true, "tools": [{"type": "function", "function": {"name": "f"}}]}),
        json!({"stream": true, "task": "embedding", "input": "x", "prompt": null}),
    ] {
        let fake = fake(Script::Done);
        let (response, server) = send(&fake, extra.clone()).await;
        let status = response.status();
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        server.abort();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{extra} {body}");
        assert_eq!(body["code"], "stream_unsupported", "{body}");
        assert!(fake.chats.lock().await.is_empty(), "nothing ran for {extra}");
    }
}

#[tokio::test]
async fn a_streamed_loop_still_stops_when_asked() {
    let fake = fake(Script::FenceLoop);
    let (response, server) = send(&fake, json!({"stream": true, "stop_on_repetition": true})).await;
    let text = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    server.abort();
    let events = events(&text);
    let (name, receipt) = events.last().unwrap();
    assert_eq!(name, "done", "{text}");
    assert_eq!(receipt["response"]["done_reason"], "repetition", "{receipt}");
}

#[tokio::test]
async fn a_client_that_disconnects_stops_generation_upstream() {
    use http_body_util::BodyExt as _;
    let fake = fake(Script::FenceLoop);
    let (response, server) = send(&fake, json!({"stream": true})).await;
    let mut body = response.into_body();
    // Read until the first generated piece, then hang up.
    let mut seen = String::new();
    while !seen.contains("event: delta") {
        let frame = body.frame().await.expect("frames until the first delta").unwrap();
        if let Ok(data) = frame.into_data() {
            seen.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
    drop(body);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sent = fake.lines_sent.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.abort();
    assert_eq!(fake.lines_sent.load(Ordering::SeqCst), sent, "generation kept going after the client left");
    assert!(sent < LOOP_LINES, "only {sent} of {LOOP_LINES} lines were generated");
}

async fn run(fake: &FakeOllama, extra: Value) -> (StatusCode, Value) {
    let (upstream, server) = serve(fake.clone()).await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", upstream, None, None)
        .with_max_queue_wait(Duration::from_millis(1500));
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), healthy).unwrap();
    let mut task = json!({"task": "completion", "objective": "fastest", "model": "loop:latest",
        "prompt": "read the invoice", "request_options": {"options": {"num_predict": 2048}}});
    for (key, value) in extra.as_object().unwrap() {
        task[key] = value.clone();
    }
    let response = app(&config)
        .unwrap()
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(task.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
        .unwrap_or(Value::Null);
    // Give the fake a moment to notice the closed connection.
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.abort();
    (status, body)
}

fn fake(script: Script) -> FakeOllama {
    FakeOllama {
        script,
        chats: Arc::default(),
        lines_sent: Arc::default(),
    }
}

#[tokio::test]
async fn a_looping_stream_is_stopped_early_and_keeps_every_generated_token() {
    let fake = fake(Script::FenceLoop);
    let (status, body) = run(&fake, json!({"stop_on_repetition": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fake.chats.lock().await[0]["stream"],
        true,
        "the request streams"
    );
    let response = &body["response"];
    assert_eq!(response["done"], true, "{body}");
    assert_eq!(response["done_reason"], "repetition", "{body}");
    let content = response["message"]["content"].as_str().unwrap();
    assert!(
        content.starts_with("Invoice #48213\nTotal: $1,337.42\n"),
        "{content}"
    );
    let tokens = response["eval_count"].as_u64().unwrap();
    assert_eq!(
        usize::try_from(tokens).unwrap(),
        content.matches('\n').count(),
        "every streamed token is kept: one line per token here"
    );
    assert!(
        tokens < 100,
        "stopped within a few checks, not at the cap: {tokens}"
    );
    let sent = fake.lines_sent.load(Ordering::SeqCst);
    assert!(
        sent < LOOP_LINES / 2,
        "the closed connection stopped the fake stream: {sent} lines sent"
    );
    let check = &body["output_check"];
    assert_eq!(check["stopped_early"], true, "{body}");
    assert_eq!(check["stop_reason"], "repetition");
    assert!(
        check["unique_prefix_chars"].as_u64().unwrap() <= 36,
        "{check}"
    );
    assert!(
        body["metrics"]["output_tokens_per_second"].is_null(),
        "a stopped stream has no Ollama timings and trains no throughput: {}",
        body["metrics"]
    );
}

#[tokio::test]
async fn ollama_repeat_limit_keeps_the_text_generated_before_it() {
    let fake = fake(Script::RepeatLimit);
    let (status, body) = run(&fake, json!({"stop_on_repetition": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let response = &body["response"];
    assert_eq!(response["done_reason"], "repeat_limit", "{body}");
    assert_eq!(
        response["message"]["content"],
        "Invoice #48213\nTotal: $1,337.42\n"
    );
    assert_eq!(body["output_check"]["stopped_early"], true, "{body}");
}

#[tokio::test]
async fn malformed_middle_frame_cannot_be_hidden_by_a_later_terminal_frame() {
    let fake = fake(Script::MalformedThenDone);
    let (status, body) = run(&fake, json!({"stop_on_repetition":true})).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_error", "{body}");
    assert_eq!(body["execution_outcome"], "unknown");
    assert_eq!(body["retry_safe"], false);
    assert_eq!(body["upstream_response"]["done"], false);
    assert_eq!(
        body["upstream_response"]["message"]["content"],
        "Invoice #48213\nTotal: $1,337.42\n"
    );
    assert_eq!(
        fake.chats.lock().await.len(),
        1,
        "accepted work must not replay"
    );
}

#[tokio::test]
async fn a_capped_task_without_the_flag_stays_non_streaming() {
    let fake = fake(Script::FenceLoop);
    let (status, body) = run(&fake, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fake.chats.lock().await[0]["stream"], false);
    assert_eq!(body["response"]["message"]["content"], "ok");
    assert!(body.get("output_check").is_none(), "{body}");
}

/// With no `num_predict`, only a loop stop bounds a runaway answer short of the model's context,
/// so it is on by default there; `stop_on_repetition: false` turns it off.
#[tokio::test]
async fn an_uncapped_task_stops_a_loop_by_default_unless_the_caller_opts_out() {
    let uncapped = json!({"request_options": {"options": {}}});
    let fake_default = fake(Script::FenceLoop);
    let (status, body) = run(&fake_default, uncapped.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fake_default.chats.lock().await[0]["stream"], true);
    assert!(
        fake_default.chats.lock().await[0]["options"]
            .get("num_predict")
            .is_none()
    );
    assert_eq!(body["response"]["done_reason"], "repetition", "{body}");

    let fake_off = fake(Script::FenceLoop);
    let mut opted_out = uncapped;
    opted_out["stop_on_repetition"] = json!(false);
    let (status, body) = run(&fake_off, opted_out).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fake_off.chats.lock().await[0]["stream"], false);

    // JSON can be legitimately periodic: a structured answer is not cut by default.
    let fake_json = fake(Script::FenceLoop);
    let (status, body) = run(
        &fake_json,
        json!({"request_options": {"options": {}, "format": "json"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fake_json.chats.lock().await[0]["stream"], false);
}

/// Loops are recorded per model and task from real runs and exposed in the catalog, which is
/// what automatic routing ranks by instead of a model's name.
#[tokio::test]
async fn observed_loops_are_recorded_and_listed_per_task() {
    let fake = fake(Script::FenceLoop);
    let (upstream, server) = serve(fake.clone()).await;
    let mut config = PlatformConfig::new("127.0.0.1:11435", upstream, None, None)
        .with_max_queue_wait(Duration::from_millis(1500));
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), healthy).unwrap();
    let platform = app(&config).unwrap();
    let task = json!({"task": "completion", "objective": "fastest", "model": "loop:latest",
        "prompt": "read the invoice", "stop_on_repetition": true,
        "request_options": {"options": {"num_predict": 2048}}});
    for _ in 0..2 {
        let response = platform
            .clone()
            .oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(task.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let response = platform
        .oneshot(
            Request::get("/_freellama/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let catalog: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    server.abort();
    let model = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["name"] == "loop:latest")
        .unwrap();
    assert_eq!(
        model["observed_loops"]["completion"],
        json!({"runs": 2, "loops": 2}),
        "{model}"
    );
}
