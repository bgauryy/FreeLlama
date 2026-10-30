//! `stop_on_repetition`: a managed chat streams from Ollama and stops as soon as the output
//! loops, keeping everything generated; without the flag the request stays non-streaming.
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
                    "/api/ps" => respond_json(&mut stream, &json!({"models": []})).await,
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
async fn without_the_flag_the_request_stays_non_streaming() {
    let fake = fake(Script::FenceLoop);
    let (status, body) = run(&fake, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fake.chats.lock().await[0]["stream"], false);
    assert_eq!(body["response"]["message"]["content"], "ok");
    assert!(body.get("output_check").is_none(), "{body}");
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
