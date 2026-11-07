//! Model use leases and the backend load lock. A model in use is never evicted; a load of one
//! model does not wait for decoding on another; cold loads on one backend are serialized;
//! and a same-model arrival behind a load shares the loaded runner.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use freellama::platform::{
    app,
    resources::{HostResources, LowMemoryMode, MemoryPressure, ResourceGovernor, ResourcePolicy},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

const SIZE: u64 = 3000;

/// One `/api/chat` call that reached Ollama. `load` completes the model's load (the model then
/// appears in `/api/ps`); `finish` ends the decode. A model that is already loaded ignores `load`.
struct Chat {
    model: String,
    load: oneshot::Sender<()>,
    finish: oneshot::Sender<()>,
}

struct Ollama {
    chats: mpsc::UnboundedReceiver<Chat>,
    loaded: Arc<Mutex<Vec<String>>>,
    unloads: Arc<Mutex<Vec<String>>>,
    platform: Router,
    server: tokio::task::JoinHandle<()>,
}

fn governor(available: Arc<AtomicU64>, low_memory: LowMemoryMode) -> ResourceGovernor {
    ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            low_memory,
            // The default reserve floors (1 GiB) dwarf this miniature host: with them, no load
            // could ever fit. Keep the percentage reserve (15% of 10 000 bytes).
            hold_available_min_bytes: 100,
            resume_available_min_bytes: 200,
            ..ResourcePolicy::default()
        },
        move || HostResources {
            source: "injected_contract".into(),
            total_memory_bytes: Some(10_000),
            available_memory_bytes: Some(available.load(Ordering::SeqCst)),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            ..HostResources::default()
        },
    )
    .unwrap()
}

/// The installed models and their metadata; both stay the same while a test runs.
fn catalog_routes(with_kv_shape: bool) -> Router {
    Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [
                    {"name": "a:latest", "digest": "digest-a", "size": SIZE},
                    {"name": "b:latest", "digest": "digest-b", "size": SIZE},
                ]}))
            }),
        )
        .route(
            "/api/show",
            post(move || async move {
                let mut info =
                    json!({"general.architecture": "llama", "llama.context_length": 32768});
                if with_kv_shape {
                    info["llama.embedding_length"] = json!(1);
                    info["llama.block_count"] = json!(1);
                    info["llama.attention.head_count"] = json!(1);
                    info["llama.attention.head_count_kv"] = json!(1);
                }
                Json(json!({"capabilities": ["completion"], "model_info": info}))
            }),
        )
}

/// An Ollama with `a:latest` and `b:latest` installed and `resident` already loaded. Unloading a
/// model returns its memory to `available`; loading one on `/api/chat` is controlled per call.
async fn ollama(resident: &[&str], available: u64, low_memory: LowMemoryMode) -> Ollama {
    ollama_with_kv_shape(resident, available, low_memory, false).await
}

async fn ollama_with_kv_shape(
    resident: &[&str],
    available: u64,
    low_memory: LowMemoryMode,
    with_kv_shape: bool,
) -> Ollama {
    let loaded = Arc::new(Mutex::new(
        resident
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    ));
    let unloads = Arc::new(Mutex::new(Vec::<String>::new()));
    let available = Arc::new(AtomicU64::new(available));
    let (send, chats) = mpsc::unbounded_channel::<Chat>();
    let (ps_loaded, chat_loaded, generate_loaded) =
        (loaded.clone(), loaded.clone(), loaded.clone());
    let (generate_unloads, generate_available) = (unloads.clone(), available.clone());
    let chat_available = available.clone();
    let router = catalog_routes(with_kv_shape)
        .route(
            "/api/ps",
            get(move || {
                let loaded = ps_loaded.clone();
                async move {
                    let models: Vec<Value> = loaded
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| {
                            json!({"name": name, "model": name, "digest": format!("digest-{}", &name[..1]),
                                "context_length": 2048, "size": SIZE,
                                "size_vram": if with_kv_shape { 0 } else { SIZE }})
                        })
                        .collect();
                    Json(json!({"models": models}))
                }
            }),
        )
        .route(
            "/api/generate",
            post(move |Json(body): Json<Value>| {
                let (loaded, unloads, available) = (
                    generate_loaded.clone(),
                    generate_unloads.clone(),
                    generate_available.clone(),
                );
                async move {
                    if body["keep_alive"] == 0 {
                        let name = body["model"].as_str().unwrap_or_default().to_owned();
                        let mut loaded = loaded.lock().unwrap();
                        if let Some(index) = loaded.iter().position(|loaded| *loaded == name) {
                            loaded.remove(index);
                            available.fetch_add(SIZE, Ordering::SeqCst);
                        }
                        unloads.lock().unwrap().push(name);
                    }
                    Json(json!({"done": true, "response": ""}))
                }
            }),
        )
        .route(
            "/api/chat",
            post(move |Json(body): Json<Value>| {
                let (send, loaded, available) =
                    (send.clone(), chat_loaded.clone(), chat_available.clone());
                async move {
                    let model = body["model"].as_str().unwrap_or_default().to_owned();
                    let (load, load_done) = oneshot::channel();
                    let (finish, finished) = oneshot::channel();
                    let present = loaded.lock().unwrap().contains(&model);
                    send.send(Chat { model: model.clone(), load, finish }).unwrap();
                    if !present {
                        let _ = load_done.await;
                        loaded.lock().unwrap().push(model);
                        available.fetch_sub(SIZE, Ordering::SeqCst);
                    }
                    let _ = finished.await;
                    Json(json!({"message": {"role": "assistant", "content": "ok"}, "done": true,
                        "prompt_eval_count": 1, "eval_count": 1}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let _ = generate_loaded;
    let mut config = crate::support::platform_config("127.0.0.1:11435", upstream, None, None)
        // A completion costs 2 units and every task of a test holds its units until it ends; the
        // budget must admit them all, since these tests are about lease sharing, not admission.
        .with_max_concurrent_tasks(16)
        .with_max_queue_wait(Duration::from_secs(5));
    config.resource_governor = governor(available, low_memory);
    Ollama {
        chats,
        loaded,
        unloads,
        platform: app(&config).unwrap(),
        server,
    }
}

type Receipt = tokio::task::JoinHandle<(StatusCode, Value)>;

/// Submit a one-shot completion for `model` without waiting for it.
fn task(platform: &Router, model: &str) -> Receipt {
    let platform = platform.clone();
    let body =
        json!({"task": "completion", "objective": "fastest", "model": model, "prompt": "hi"});
    tokio::spawn(async move {
        let response = platform
            .oneshot(
                Request::post("/_freellama/v1/tasks")
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
    })
}

async fn next_chat(chats: &mut mpsc::UnboundedReceiver<Chat>, model: &str) -> Chat {
    let chat = tokio::time::timeout(Duration::from_secs(3), chats.recv())
        .await
        .unwrap_or_else(|_| panic!("{model} never reached Ollama"))
        .unwrap();
    assert_eq!(chat.model, model);
    chat
}

/// True when nothing reaches Ollama within `window`.
async fn stays_quiet(chats: &mut mpsc::UnboundedReceiver<Chat>, window: Duration) -> bool {
    tokio::time::timeout(window, chats.recv()).await.is_err()
}

const QUIET: Duration = Duration::from_millis(400);

#[tokio::test]
async fn a_cold_load_proceeds_while_another_model_decodes_when_memory_admits_it() {
    let mut ollama = ollama(&["a:latest"], 10_000, LowMemoryMode::Advisory).await;
    let a = task(&ollama.platform, "a:latest");
    let a_chat = next_chat(&mut ollama.chats, "a:latest").await;

    let b = task(&ollama.platform, "b:latest");
    let b_chat = next_chat(&mut ollama.chats, "b:latest").await;
    assert!(
        ollama.unloads.lock().unwrap().is_empty(),
        "an admitted load evicts nothing"
    );
    b_chat.load.send(()).unwrap();
    b_chat.finish.send(()).unwrap();
    let (status, receipt) = b.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(
        receipt["admission"]["mode"],
        "nonresident_transition_exclusive"
    );
    assert!(
        !a.is_finished(),
        "A was still decoding while B loaded and ran"
    );

    a_chat.finish.send(()).unwrap();
    let (status, receipt) = a.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["admission"]["mode"], "resident_shared");
    ollama.server.abort();
}

#[tokio::test]
async fn a_load_that_needs_room_waits_for_the_busy_model_and_never_evicts_it_mid_use() {
    // B's load needs 4050 bytes (3000 x 1.35) plus a 1500-byte reserve. 3500 available cannot
    // hold that next to A, but A's 3000 returned by an unload (6500) can.
    let mut ollama = ollama(&["a:latest"], 3500, LowMemoryMode::Refuse).await;
    let a = task(&ollama.platform, "a:latest");
    let a_chat = next_chat(&mut ollama.chats, "a:latest").await;

    let b = task(&ollama.platform, "b:latest");
    assert!(
        stays_quiet(&mut ollama.chats, QUIET).await,
        "B must wait while A is in use"
    );
    assert!(
        ollama.unloads.lock().unwrap().is_empty(),
        "A is never unloaded while a task is using it"
    );
    assert!(
        ollama
            .loaded
            .lock()
            .unwrap()
            .contains(&"a:latest".to_owned())
    );

    a_chat.finish.send(()).unwrap();
    let (status, receipt) = a.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");

    let b_chat = next_chat(&mut ollama.chats, "b:latest").await;
    assert_eq!(
        *ollama.unloads.lock().unwrap(),
        ["a:latest"],
        "A is evicted once idle, before B loads"
    );
    b_chat.load.send(()).unwrap();
    b_chat.finish.send(()).unwrap();
    let (status, receipt) = b.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    ollama.server.abort();
}

#[tokio::test]
async fn same_model_arrivals_during_a_load_wait_for_it_and_then_share_the_runner() {
    let mut ollama = ollama(&[], 10_000, LowMemoryMode::Advisory).await;
    let first = task(&ollama.platform, "b:latest");
    let first_chat = next_chat(&mut ollama.chats, "b:latest").await;

    let second = task(&ollama.platform, "b:latest");
    let third = task(&ollama.platform, "b:latest");
    assert!(
        stays_quiet(&mut ollama.chats, QUIET).await,
        "arrivals for a model that is loading wait; they do not load it again"
    );

    first_chat.load.send(()).unwrap();
    let second_chat = next_chat(&mut ollama.chats, "b:latest").await;
    let third_chat = next_chat(&mut ollama.chats, "b:latest").await;
    assert!(
        !first.is_finished(),
        "the waiters share the runner while the loader is still decoding"
    );
    for chat in [first_chat.finish, second_chat.finish, third_chat.finish] {
        chat.send(()).unwrap();
    }
    let (status, loader) = first.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{loader}");
    assert_eq!(
        loader["admission"]["mode"],
        "nonresident_transition_exclusive"
    );
    for waiter in [second, third] {
        let (status, receipt) = waiter.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["admission"]["mode"], "resident_shared");
    }
    ollama.server.abort();
}

#[tokio::test]
async fn strict_same_model_waiter_remeasures_when_the_loader_is_ready_before_decode_ends() {
    // One 4050-byte forecast plus the reserve fits; two do not. Loading consumes 3000 bytes
    // of actual host availability, so merely releasing/retrying the first forecast is not enough.
    let mut ollama = ollama(&[], 7000, LowMemoryMode::Refuse).await;
    let first = task(&ollama.platform, "b:latest");
    let first_chat = next_chat(&mut ollama.chats, "b:latest").await;
    let second = task(&ollama.platform, "b:latest");
    assert!(
        stays_quiet(&mut ollama.chats, Duration::from_millis(2300)).await,
        "the follower must wait for the in-progress runner, beyond the slot-held resource wait"
    );

    first_chat.load.send(()).unwrap();
    let second_chat = next_chat(&mut ollama.chats, "b:latest").await;
    assert!(
        !first.is_finished(),
        "readiness must wake the follower before decode ends"
    );
    assert!(ollama.unloads.lock().unwrap().is_empty());
    first_chat.finish.send(()).unwrap();
    second_chat.finish.send(()).unwrap();
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    let (status, receipt) = second.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["admission"]["mode"], "resident_shared");
    ollama.server.abort();
}

#[tokio::test]
async fn raw_nonstream_load_releases_the_load_lock_before_response_headers() {
    let mut ollama = ollama(&[], 10_000, LowMemoryMode::Advisory).await;
    let platform = ollama.platform.clone();
    let raw = tokio::spawn(async move {
        platform
            .oneshot(
                Request::post("/api/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model":"b:latest", "messages":[{"role":"user","content":"hi"}],
                            "stream":false, "options":{"num_ctx":2048}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    let raw_chat = next_chat(&mut ollama.chats, "b:latest").await;
    let managed = task(&ollama.platform, "a:latest");
    assert!(stays_quiet(&mut ollama.chats, QUIET).await);
    raw_chat.load.send(()).unwrap();

    let managed_chat = next_chat(&mut ollama.chats, "a:latest").await;
    assert!(
        !raw.is_finished(),
        "the upstream has not returned nonstream response headers"
    );
    assert!(ollama.unloads.lock().unwrap().is_empty());
    managed_chat.load.send(()).unwrap();
    managed_chat.finish.send(()).unwrap();
    assert_eq!(managed.await.unwrap().0, StatusCode::OK);
    raw_chat.finish.send(()).unwrap();
    let response = raw.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), usize::MAX).await.unwrap();
    ollama.server.abort();
}

#[tokio::test]
async fn raw_context_forecast_makes_room_even_when_the_file_only_estimate_fits() {
    // The file-only forecast is 4050 and clears 6000 available with the 1500 reserve. The known
    // KV shape at 1024 tokens needs about 7300 including weights/graph, so the idle 3000-byte
    // runner must be evicted before the raw request can fit. Its allocation is in host memory.
    let mut ollama = ollama_with_kv_shape(&["a:latest"], 6000, LowMemoryMode::Refuse, true).await;
    let platform = ollama.platform.clone();
    let raw = tokio::spawn(async move {
        platform
            .oneshot(
                Request::post("/api/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model":"b:latest", "messages":[{"role":"user","content":"hi"}],
                            "stream":false, "options":{"num_ctx":1024}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    let chat = next_chat(&mut ollama.chats, "b:latest").await;
    assert_eq!(*ollama.unloads.lock().unwrap(), ["a:latest"]);
    chat.load.send(()).unwrap();
    chat.finish.send(()).unwrap();
    let response = raw.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    to_bytes(response.into_body(), usize::MAX).await.unwrap();
    ollama.server.abort();
}

#[tokio::test]
async fn a_cancelled_loader_releases_the_load_lock_to_the_next_load() {
    let mut ollama = ollama(&[], 10_000, LowMemoryMode::Advisory).await;
    let loader = task(&ollama.platform, "a:latest");
    let _loader_chat = next_chat(&mut ollama.chats, "a:latest").await;

    let waiting = task(&ollama.platform, "b:latest");
    assert!(
        stays_quiet(&mut ollama.chats, QUIET).await,
        "a second model's load waits behind the first"
    );

    loader.abort();
    assert!(loader.await.unwrap_err().is_cancelled());
    let waiting_chat = next_chat(&mut ollama.chats, "b:latest").await;
    waiting_chat.load.send(()).unwrap();
    waiting_chat.finish.send(()).unwrap();
    let (status, receipt) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{receipt}");
    ollama.server.abort();
}
