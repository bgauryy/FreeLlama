use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
};

use freellama::{
    model_bench::Capability,
    platform::{
        CatalogModel, Objective, PlatformConfig, RouteInput, RouteIntent, SessionAffinity,
        TaskKind, app, intent_schema, normalize_route_intent, parse_route_intent, select_route,
    },
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;

fn candidate(name: &str, size: u64, capabilities: &[Capability], resident: bool) -> CatalogModel {
    CatalogModel {
        name: name.to_owned(),
        size,
        capabilities: capabilities.iter().copied().collect(),
        advertised_context: Some(32_768),
        resident,
        resident_vram: resident.then_some(size),
        benchmark: BTreeMap::new(),
        policy_rank: [
            TaskKind::Completion,
            TaskKind::Coding,
            TaskKind::CodeRepair,
            TaskKind::Tools,
            TaskKind::Browser,
            TaskKind::Vision,
            TaskKind::Embedding,
            TaskKind::LongContext,
        ]
        .into_iter()
        .map(|task| (task, 0))
        .collect(),
    }
}

#[tokio::test]
async fn embedding_task_forwards_route_options_and_returns_prompt_free_metrics() {
    let captured = Arc::new(Mutex::new(None));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route(
            "/api/ps",
            get(|| async { Json(json!({"models": []})) }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/embed",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().await = Some(body);
                    Json(json!({
                        "embeddings": [[0.1, 0.2]],
                        "total_duration": 2_000_000_000_u64,
                        "load_duration": 500_000_000_u64,
                        "prompt_eval_count": 10,
                        "prompt_eval_duration": 1_000_000_000_u64
                    }))
                },
            ),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"task":"embedding","objective":"fastest","model":"embed-model","input":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let upstream_body = captured.lock().await.clone().unwrap();
    assert_eq!(upstream_body["options"]["num_ctx"], 2048);
    assert_eq!(response["metrics"]["prompt_tokens_per_second"], 10.0);
    assert_eq!(response["metrics"]["load_duration_ns"], 500_000_000_u64);
    assert!(response["metrics"].get("response").is_none());
    assert_eq!(
        response["admission"]["mode"],
        "nonresident_transition_exclusive"
    );
    assert_eq!(upstream_body["keep_alive"], "5m");
    mock_task.abort();
}

#[tokio::test]
async fn embedding_task_honors_an_explicit_keep_alive_override() {
    let captured = Arc::new(Mutex::new(None));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/embed",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().await = Some(body);
                    Json(json!({"embeddings": [[0.1, 0.2]]}))
                },
            ),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"task":"embedding","objective":"fastest","model":"embed-model","input":"hello","keep_alive":"0"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let upstream_body = captured.lock().await.clone().unwrap();
    assert_eq!(
        upstream_body["keep_alive"], "0",
        "an explicit keep_alive must reach Ollama verbatim, not the default"
    );
    mock_task.abort();
}

#[tokio::test]
async fn prompt_task_forwards_images_onto_the_built_message() {
    let captured = Arc::new(Mutex::new(None));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "vision-model", "size": 1_000_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["completion", "vision"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/chat",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().await = Some(body);
                    Json(json!({"message": {"role": "assistant", "content": "a red square"}}))
                },
            ),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"task":"vision","objective":"fastest","model":"vision-model","prompt":"what color?","images":["dGVzdA=="]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let upstream_body = captured.lock().await.clone().unwrap();
    let sent_messages = upstream_body["messages"].as_array().unwrap();
    assert_eq!(sent_messages.len(), 1);
    assert_eq!(sent_messages[0]["content"], "what color?");
    assert_eq!(sent_messages[0]["images"], json!(["dGVzdA=="]));
    mock_task.abort();
}

#[tokio::test]
async fn prompt_task_without_images_sends_no_images_field() {
    let captured = Arc::new(Mutex::new(None));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|| async { Json(json!({"capabilities": ["completion"], "model_info": {}})) }),
        )
        .route(
            "/api/chat",
            post(
                |State(captured): State<Arc<Mutex<Option<Value>>>>,
                 Json(body): Json<Value>| async move {
                    *captured.lock().await = Some(body);
                    Json(json!({"message": {"role": "assistant", "content": "hi"}}))
                },
            ),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"task":"completion","objective":"fastest","model":"embed-model","prompt":"hi"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let upstream_body = captured.lock().await.clone().unwrap();
    let sent_messages = upstream_body["messages"].as_array().unwrap();
    assert!(
        sent_messages[0].get("images").is_none(),
        "no images provided — must not send an images field at all, not send it empty"
    );
    mock_task.abort();
}

#[derive(Clone, Default)]
struct ConcurrencyProbe {
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

#[tokio::test]
async fn nonresident_managed_tasks_serialize_upstream_execution() {
    let probe = ConcurrencyProbe::default();
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/embed",
            post(
                |State(probe): State<ConcurrencyProbe>, Json(_): Json<Value>| async move {
                    let active = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
                    probe.maximum.fetch_max(active, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    probe.active.fetch_sub(1, Ordering::SeqCst);
                    Json(json!({"embeddings": [[0.1]]}))
                },
            ),
        )
        .with_state(probe.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();
    let request = || {
        Request::post("/_freellama/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"task":"embedding","objective":"fastest","model":"embed-model","input":"hello"}"#,
            ))
            .unwrap()
    };

    let (first, second) = tokio::join!(
        platform.clone().oneshot(request()),
        platform.oneshot(request())
    );

    assert_eq!(first.unwrap().status(), StatusCode::OK);
    assert_eq!(second.unwrap().status(), StatusCode::OK);
    assert_eq!(probe.maximum.load(Ordering::SeqCst), 1);
    mock_task.abort();
}

#[tokio::test]
async fn resident_managed_tasks_keep_same_model_concurrency() {
    let probe = ConcurrencyProbe::default();
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route(
            "/api/ps",
            get(|| async {
                Json(json!({
                    "models": [{"name": "embed-model", "size_vram": 274_000_000}]
                }))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/embed",
            post(
                |State(probe): State<ConcurrencyProbe>, Json(_): Json<Value>| async move {
                    let active = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
                    probe.maximum.fetch_max(active, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    probe.active.fetch_sub(1, Ordering::SeqCst);
                    Json(json!({"embeddings": [[0.1]]}))
                },
            ),
        )
        .with_state(probe.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();
    let request = || {
        Request::post("/_freellama/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"task":"embedding","objective":"fastest","model":"embed-model","input":"hello"}"#,
            ))
            .unwrap()
    };

    let (first, second) = tokio::join!(
        platform.clone().oneshot(request()),
        platform.oneshot(request())
    );

    assert_eq!(first.unwrap().status(), StatusCode::OK);
    assert_eq!(second.unwrap().status(), StatusCode::OK);
    assert_eq!(probe.maximum.load(Ordering::SeqCst), 2);
    mock_task.abort();
}

#[test]
fn route_filters_by_capability_before_speed() {
    let mut fast_text = candidate("fast-text", 2_000_000_000, &[Capability::Completion], true);
    fast_text.benchmark.insert(Capability::Completion, 50_000.0);
    let mut tool_model = candidate(
        "tool-model",
        8_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    );
    tool_model.benchmark.insert(Capability::Tools, 2_000.0);

    let route = select_route(
        &RouteInput {
            task: TaskKind::Tools,
            objective: Objective::Balanced,
            ..RouteInput::default()
        },
        &[fast_text, tool_model],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(route.selected_model, "tool-model");
    assert!(route.required_capabilities.contains(&Capability::Tools));
}

#[test]
fn an_eligible_session_model_wins_without_crossing_sessions() {
    let mut first = candidate("first", 4_000_000_000, &[Capability::Completion], false);
    first.benchmark.insert(Capability::Completion, 1_000.0);
    let mut second = candidate("second", 4_000_000_000, &[Capability::Completion], true);
    second.benchmark.insert(Capability::Completion, 2_000.0);
    let affinity = SessionAffinity::from_pairs([
        ("session-a".to_owned(), "first".to_owned()),
        ("session-b".to_owned(), "second".to_owned()),
    ]);

    let route = select_route(
        &RouteInput {
            session_id: Some("session-a".to_owned()),
            ..RouteInput::default()
        },
        &[first, second],
        &affinity,
    )
    .unwrap();

    assert_eq!(route.selected_model, "first");
    assert!(
        route
            .reasons
            .iter()
            .any(|reason| reason == "session_affinity")
    );
}

#[test]
fn browser_task_returns_a_bounded_action_configuration() {
    let model = candidate(
        "browser-model",
        10_000_000_000,
        &[
            Capability::Completion,
            Capability::Tools,
            Capability::Thinking,
        ],
        false,
    );

    let route = select_route(
        &RouteInput {
            task: TaskKind::Browser,
            ..RouteInput::default()
        },
        &[model],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(route.profile, "browser_action");
    assert_eq!(route.options["num_predict"], 64);
    assert_eq!(route.options["num_ctx"], 8192);
    assert_eq!(route.think, serde_json::Value::Bool(false));
    assert!(!route.stream);
    assert!(route.strict_tool_validation);
}

#[test]
fn explicit_model_must_be_installed_and_eligible() {
    let model = candidate("text-only", 2_000_000_000, &[Capability::Completion], false);
    let result = select_route(
        &RouteInput {
            task: TaskKind::Vision,
            model: Some("text-only".to_owned()),
            ..RouteInput::default()
        },
        &[model],
        &SessionAffinity::default(),
    );
    assert!(result.unwrap_err().to_string().contains("not eligible"));
}

#[test]
fn caller_requirements_extend_the_task_contract() {
    let model = candidate(
        "multimodal",
        8_000_000_000,
        &[
            Capability::Completion,
            Capability::Tools,
            Capability::Vision,
        ],
        false,
    );
    let route = select_route(
        &RouteInput {
            required_capabilities: BTreeSet::from([Capability::Vision]),
            ..RouteInput::default()
        },
        &[model],
        &SessionAffinity::default(),
    )
    .unwrap();
    assert!(
        route
            .required_capabilities
            .contains(&Capability::Completion)
    );
    assert!(route.required_capabilities.contains(&Capability::Vision));
}

#[test]
fn balanced_routing_refuses_an_unqualified_functional_winner() {
    let mut tiny = candidate(
        "tiny",
        500_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    );
    tiny.benchmark.insert(Capability::Tools, 50_000.0);
    tiny.policy_rank.clear();

    let result = select_route(
        &RouteInput {
            task: TaskKind::Browser,
            objective: Objective::Balanced,
            ..RouteInput::default()
        },
        &[tiny],
        &SessionAffinity::default(),
    );

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no quality-qualified model")
    );
}

#[test]
fn coding_disables_thinking_when_the_model_does_not_advertise_it() {
    let model = candidate(
        "plain-completion",
        8_000_000_000,
        &[Capability::Completion],
        false,
    );
    let route = select_route(
        &RouteInput {
            task: TaskKind::Coding,
            model: Some("plain-completion".to_owned()),
            ..RouteInput::default()
        },
        &[model],
        &SessionAffinity::default(),
    )
    .unwrap();
    assert_eq!(route.think, serde_json::Value::Bool(false));
}

#[test]
fn qwen_code_repair_uses_the_measured_agent_profile() {
    let model = candidate(
        "qwen3.8:27b-mlx",
        18_174_721_847,
        &[
            Capability::Completion,
            Capability::Tools,
            Capability::Thinking,
        ],
        false,
    );
    let route = select_route(
        &RouteInput {
            task: TaskKind::CodeRepair,
            objective: Objective::Quality,
            ..RouteInput::default()
        },
        &[model],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(route.selected_model, "qwen3.8:27b-mlx");
    assert_eq!(route.profile, "qwen_code_repair");
    assert_eq!(route.options["num_ctx"], 8192);
    assert_eq!(route.options["num_predict"], 512);
    assert_eq!(route.think, serde_json::Value::Bool(false));
    assert!(route.strict_tool_validation);
    assert!(route.required_capabilities.contains(&Capability::Tools));
}

#[test]
fn platform_is_loopback_only() {
    let config = PlatformConfig::new(
        "0.0.0.0:11435",
        "http://127.0.0.1:11434",
        None,
        None,
        "qwen2.5:0.5b",
    );
    assert!(config.validate().is_err());
}

#[test]
fn natural_language_schema_cannot_select_a_model_directly() {
    let schema = intent_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema["properties"].get("model").is_none());
    assert_eq!(
        schema["properties"]["task"]["enum"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert_eq!(schema["properties"]["context_tokens"]["minimum"], 512);
}

#[test]
fn parsed_browser_intent_becomes_capability_requirements() {
    let intent = parse_route_intent(
        r#"{"task":"browser","objective":"balanced","context_tokens":8192,"requires_tools":true,"requires_vision":true}"#,
    )
    .unwrap();
    let route = intent.into_route_input(Some("session-a".to_owned()));
    assert_eq!(route.task, TaskKind::Browser);
    assert_eq!(route.session_id.as_deref(), Some("session-a"));
    assert!(route.required_capabilities.contains(&Capability::Tools));
    assert!(route.required_capabilities.contains(&Capability::Vision));
}

#[test]
fn natural_language_output_with_unknown_fields_fails_closed() {
    let result = parse_route_intent(
        r#"{"task":"completion","objective":"fastest","context_tokens":null,"requires_tools":false,"requires_vision":false,"model":"tiny"}"#,
    );
    assert!(result.is_err());
}

#[test]
fn explicit_natural_language_constraints_override_a_weak_interpreter() {
    let weak = RouteIntent {
        task: TaskKind::Completion,
        objective: Objective::Balanced,
        context_tokens: None,
        requires_tools: true,
        requires_vision: true,
    };
    let (intent, adjustments) = normalize_route_intent(
        weak,
        "Open the checkout screenshot and click the button as fast as possible.",
    );
    assert_eq!(intent.task, TaskKind::Browser);
    assert_eq!(intent.objective, Objective::Fastest);
    assert!(intent.requires_tools);
    assert!(intent.requires_vision);
    assert!(!adjustments.is_empty());
}

#[test]
fn unsupported_natural_language_requirements_are_cleared() {
    let weak = RouteIntent {
        task: TaskKind::Completion,
        objective: Objective::Fastest,
        context_tokens: Some(1),
        requires_tools: true,
        requires_vision: true,
    };
    let (intent, adjustments) =
        normalize_route_intent(weak, "Reply as fast as possible to a normal text question.");
    assert_eq!(intent.task, TaskKind::Completion);
    assert_eq!(intent.objective, Objective::Fastest);
    assert_eq!(intent.context_tokens, None);
    assert!(!intent.requires_tools);
    assert!(!intent.requires_vision);
    assert!(
        adjustments
            .iter()
            .any(|value| value == "unsupported_tool_requirement_cleared")
    );
    assert!(
        adjustments
            .iter()
            .any(|value| value == "unsupported_vision_requirement_cleared")
    );
    assert!(
        adjustments
            .iter()
            .any(|value| value == "unsupported_context_requirement_cleared")
    );
}

#[test]
fn explicit_small_context_is_clamped_and_task_requirements_are_preserved() {
    let weak = RouteIntent {
        task: TaskKind::Tools,
        objective: Objective::Balanced,
        context_tokens: Some(1),
        requires_tools: false,
        requires_vision: false,
    };
    let (intent, adjustments) =
        normalize_route_intent(weak, "Use tools with a context of one token.");
    assert_eq!(intent.context_tokens, Some(512));
    assert!(intent.requires_tools);
    assert!(
        adjustments
            .iter()
            .any(|value| value == "context_requirement_clamped")
    );
}

#[test]
fn explicit_embedding_and_quality_terms_are_deterministic() {
    let weak = RouteIntent {
        task: TaskKind::LongContext,
        objective: Objective::Fastest,
        context_tokens: None,
        requires_tools: true,
        requires_vision: true,
    };
    let (intent, _) = normalize_route_intent(
        weak,
        "Create semantic vectors with the best evaluated local setup.",
    );
    assert_eq!(intent.task, TaskKind::Embedding);
    assert_eq!(intent.objective, Objective::Quality);
    assert!(!intent.requires_tools);
    assert!(!intent.requires_vision);
}

#[test]
fn explicit_code_review_and_maximum_quality_terms_are_deterministic() {
    let weak = RouteIntent {
        task: TaskKind::LongContext,
        objective: Objective::Fastest,
        context_tokens: None,
        requires_tools: false,
        requires_vision: false,
    };
    let (intent, _) = normalize_route_intent(
        weak,
        "Review and debug a Rust codebase; prioritize maximum answer quality.",
    );
    assert_eq!(intent.task, TaskKind::Coding);
    assert_eq!(intent.objective, Objective::Quality);
}

#[test]
fn explicit_bug_fix_terms_select_code_repair_and_require_tools() {
    let weak = RouteIntent {
        task: TaskKind::Completion,
        objective: Objective::Balanced,
        context_tokens: None,
        requires_tools: false,
        requires_vision: false,
    };
    let (intent, adjustments) = normalize_route_intent(
        weak,
        "Fix the bug in this repository and edit files with the smallest repair.",
    );
    assert_eq!(intent.task, TaskKind::CodeRepair);
    assert!(intent.requires_tools);
    assert!(
        adjustments
            .iter()
            .any(|value| value == "explicit_code_repair_term")
    );
}

/// Discovery routes every managed task needs before it can pick a model. Shared by the
/// managed-task reliability tests below so each one only has to describe its own `/api/embed`
/// behaviour.
fn discovery_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route("/api/ps", get(|| async { Json(json!({"models": []})) }))
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
}

fn embedding_task_request() -> Request<Body> {
    Request::post("/_freellama/v1/tasks")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"task":"embedding","objective":"fastest","model":"embed-model","input":"hello"}"#,
        ))
        .unwrap()
}

/// Ollama returns 500 under load-model contention — the exact condition managed routing creates
/// when it transitions models. The passthrough proxy has always ridden that out; the managed path
/// used to fail the whole task on the first one, *and* throw away the admission permit it was
/// holding. One retry must be enough to succeed.
#[tokio::test]
async fn managed_task_retries_a_transient_upstream_500() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = discovery_routes()
        .route(
            "/api/embed",
            post(|State(calls): State<Arc<AtomicUsize>>| async move {
                let seen = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if seen == 1 {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "model is loading"})),
                    )
                } else {
                    (StatusCode::OK, Json(json!({"embeddings": [[0.1, 0.2]]})))
                }
            }),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform.oneshot(embedding_task_request()).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the managed task should have retried the transient 500 exactly once"
    );
    mock_task.abort();
}

/// A wedged Ollama runner does not always answer in JSON. Parsing strictly turned a truthful 500
/// into a 502 "decode error", which hid the real upstream status from the caller and pointed
/// debugging at the wrong layer. The status must survive, and the body must come through as text.
#[tokio::test]
async fn managed_task_preserves_a_non_json_upstream_error() {
    let mock = discovery_routes::<()>()
        .route(
            "/api/embed",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "<html>502 Bad Gateway</html>",
                )
            }),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform.oneshot(embedding_task_request()).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "the upstream status must survive rather than collapsing into a decode error"
    );
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("502 Bad Gateway"),
        "the upstream body should be carried through, got {body}"
    );
    mock_task.abort();
}

/// An upstream that records the maximum number of requests in flight simultaneously.
/// (requests currently in flight, high-water mark).
type InFlight = Arc<(AtomicUsize, AtomicUsize)>;

fn concurrency_probe() -> (Router<InFlight>, InFlight) {
    let counters = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))); // (in_flight, peak)
    // The model must report as RESIDENT: only then does the managed path take the shared permit.
    // A non-resident model takes the exclusive transition permit and serializes by design, which
    // would make this test measure the wrong mechanism.
    let router = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size": 274_000_000}]}))
            }),
        )
        .route(
            "/api/ps",
            get(|| async {
                Json(json!({"models": [{"name": "embed-model", "size_vram": 274_000_000}]}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["embedding"],
                    "model_info": {"test.context_length": 2048}
                }))
            }),
        )
        .route(
            "/api/embed",
            post(|State(c): State<InFlight>| async move {
                let now = c.0.fetch_add(1, Ordering::SeqCst) + 1;
                c.1.fetch_max(now, Ordering::SeqCst);
                // Long enough that genuinely-concurrent requests overlap.
                tokio::time::sleep(Duration::from_millis(120)).await;
                c.0.fetch_sub(1, Ordering::SeqCst);
                Json(json!({"embeddings": [[0.1]]}))
            }),
        );
    (router, counters)
}

/// The managed path grants resident tasks a SHARED admission permit, so without a slot bound any
/// number of them reach Ollama together. Ollama then queues them (`OLLAMA_MAX_QUEUE`, 512) and
/// 503s the overflow, while each queued request burns its own 900s budget waiting. The semaphore
/// converts that burst into bounded, FIFO backpressure.
#[tokio::test]
async fn managed_tasks_never_exceed_the_configured_concurrency() {
    let (mock, counters) = concurrency_probe();
    let mock = mock.with_state(counters.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform =
        app(
            &PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
                .with_max_concurrent_tasks(2),
        )
        .unwrap();

    // Fire eight at once against a two-slot limit.
    let mut joins = Vec::new();
    for _ in 0..8 {
        let p = platform.clone();
        joins.push(tokio::spawn(async move {
            p.oneshot(embedding_task_request()).await.unwrap().status()
        }));
    }
    for j in joins {
        assert_eq!(j.await.unwrap(), StatusCode::OK);
    }

    let peak = counters.1.load(Ordering::SeqCst);
    assert!(
        peak <= 2,
        "embeddings cost 1 unit each, so a 2-unit budget admits at most 2; saw {peak} in flight"
    );
    assert!(
        peak >= 2,
        "the budget should be usable, not serialize to 1 (saw {peak})"
    );
    mock_task.abort();
}

/// Throttling that is invisible is indistinguishable from a slow model. The caller has to be able
/// to attribute the latency, so the wait and the slot budget are reported on every response.
#[tokio::test]
async fn managed_tasks_report_their_queue_wait_and_slot_budget() {
    let mock = discovery_routes::<()>()
        .route(
            "/api/embed",
            post(|| async { Json(json!({"embeddings": [[0.1]]})) }),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform = app(&PlatformConfig::new(
        "127.0.0.1:11435",
        upstream,
        None,
        None,
        "qwen2.5:0.5b",
    ))
    .unwrap();

    let response = platform.oneshot(embedding_task_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let admission = &body["admission"];
    assert!(admission["queue_wait_ms"].is_number(), "got {admission}");
    assert!(
        admission["slots_total"].as_u64().is_some_and(|n| n > 0),
        "slot budget must be reported, got {admission}"
    );
    mock_task.abort();
}

/// Cost weighting is the point of the admission budget: a 274MB embedding and an 18GB vision
/// generation are not interchangeable. Charging both a flat 1 would let four vision calls in where
/// four embeddings belong. Vision costs 4 units, so a 4-unit budget must serialize them to one at a
/// time while admitting four embeddings together.
#[tokio::test]
async fn vision_tasks_cost_more_admission_than_embeddings() {
    let counters: InFlight = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0)));
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async {
                Json(json!({"models": [{"name": "seer", "size": 18_000_000_000_u64}]}))
            }),
        )
        .route(
            "/api/ps",
            get(|| async {
                Json(json!({"models": [{"name": "seer", "size_vram": 18_000_000_000_u64}]}))
            }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(json!({
                    "capabilities": ["completion", "vision"],
                    "model_info": {"test.context_length": 4096}
                }))
            }),
        )
        .route(
            "/api/chat",
            post(|State(c): State<InFlight>| async move {
                let now = c.0.fetch_add(1, Ordering::SeqCst) + 1;
                c.1.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(120)).await;
                c.0.fetch_sub(1, Ordering::SeqCst);
                Json(json!({"message": {"role": "assistant", "content": "seen"}}))
            }),
        )
        .with_state(counters.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform =
        app(
            &PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
                .with_max_concurrent_tasks(4),
        )
        .unwrap();

    let mut joins = Vec::new();
    for _ in 0..4 {
        let p = platform.clone();
        joins.push(tokio::spawn(async move {
            p.oneshot(
                Request::post("/_freellama/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"task":"vision","model":"seer","prompt":"describe","images":["aGk="]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }));
    }
    for j in joins {
        assert_eq!(j.await.unwrap(), StatusCode::OK);
    }

    assert_eq!(
        counters.1.load(Ordering::SeqCst),
        1,
        "vision costs the whole 4-unit budget, so the four calls must serialize"
    );
    mock_task.abort();
}

/// Saturation must refuse, not wait forever. Ollama's own scheduler does a non-blocking send onto
/// its pending channel and returns `ErrMaxQueue` ("server busy, please try again") the instant it
/// is full; an unbounded wait here would turn that honest signal into an invisible pile-up whose
/// only symptom is latency the caller cannot attribute.
#[tokio::test]
async fn a_saturated_admission_queue_refuses_instead_of_waiting_forever() {
    let mock = discovery_routes::<()>()
        .route(
            "/api/embed",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Json(json!({"embeddings": [[0.1]]}))
            }),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform =
        app(
            &PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
                .with_max_concurrent_tasks(1)
                .with_max_queue_wait(Duration::from_millis(50)),
        )
        .unwrap();

    // One task holds the single slot for 400ms; the second cannot get in within 50ms.
    let holder = {
        let p = platform.clone();
        tokio::spawn(async move { p.oneshot(embedding_task_request()).await.unwrap().status() })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;
    let refused = platform.oneshot(embedding_task_request()).await.unwrap();

    assert_eq!(
        refused.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a saturated queue must refuse fast, matching Ollama's ErrMaxQueue contract"
    );
    let body: Value =
        serde_json::from_slice(&to_bytes(refused.into_body(), usize::MAX).await.unwrap()).unwrap();
    let message = body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("server busy") && message.contains("cost"),
        "the refusal should name the cost and budget so it is actionable, got {message:?}"
    );
    assert_eq!(
        holder.await.unwrap(),
        StatusCode::OK,
        "the holder still completes"
    );
    mock_task.abort();
}

/// The confidence gate must live in the core, not in one consumer.
///
/// It was implemented only in the TypeScript MCP wrapper, so `freellama route`, the HTTP API and
/// anyone embedding `freellama-core` as a library got a router with no fail-closed protection —
/// while the README described `minConfidence` as a property of the platform. Enforcing it inside
/// `select_route` makes every caller inherit it.
#[test]
fn min_confidence_is_enforced_by_the_router_itself() {
    let models = vec![candidate(
        "qwen3.8:27b-mlx",
        18_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    )];
    let input = RouteInput {
        task: TaskKind::CodeRepair,
        objective: Objective::Fastest,
        min_confidence: Some("medium".to_owned()),
        ..RouteInput::default()
    };

    let refused = select_route(&input, &models, &SessionAffinity::default());
    let error = refused.expect_err("a low-confidence route must be refused, not returned");
    let text = error.to_string();
    assert!(text.contains("route refused"), "got {text}");
    assert!(
        text.contains("evidence:"),
        "the refusal must name the evidence behind the grade, got {text}"
    );
    assert!(
        text.contains("qwen3.8:27b-mlx"),
        "the refusal must name the model it would have picked, got {text}"
    );
    assert!(
        text.contains("bench-all") && text.contains("policy-from-eval"),
        "the refusal must name both inputs that raise the grade, got {text}"
    );
}

/// Without the gate the same route succeeds — proving the refusal is caused by `min_confidence`
/// and not by the candidate being ineligible for some other reason.
#[test]
fn the_same_route_succeeds_when_no_minimum_is_requested() {
    let models = vec![candidate(
        "qwen3.8:27b-mlx",
        18_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    )];
    let input = RouteInput {
        task: TaskKind::CodeRepair,
        objective: Objective::Fastest,
        ..RouteInput::default()
    };

    let decision = select_route(&input, &models, &SessionAffinity::default()).unwrap();
    assert_eq!(decision.selected_model, "qwen3.8:27b-mlx");
    assert_eq!(decision.confidence, "low");
}

/// An unrecognised *minimum* must fail closed, not open.
///
/// Ranking an unknown string lowest silently disables the gate: `min_confidence: "high"` — the
/// most natural typo, since "high" is the grade this router does not issue — would have accepted
/// every "low" route while looking exactly like a satisfied floor. The CLI takes this value as a
/// free-form string, so nothing else validates it.
#[test]
fn an_unknown_confidence_grade_fails_closed() {
    let models = vec![candidate(
        "qwen3.8:27b-mlx",
        18_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    )];
    let input = RouteInput {
        task: TaskKind::CodeRepair,
        objective: Objective::Fastest,
        min_confidence: Some("high".to_owned()),
        ..RouteInput::default()
    };
    let error = select_route(&input, &models, &SessionAffinity::default())
        .expect_err("an unknown minimum must refuse, never silently accept everything");
    let text = error.to_string();
    assert!(
        text.contains("unknown min_confidence") && text.contains("high"),
        "the refusal must name the unusable value, got {text}"
    );
}

/// The grades the router does issue are still accepted as minimums.
#[test]
fn a_low_minimum_accepts_a_low_route() {
    let models = vec![candidate(
        "qwen3.8:27b-mlx",
        18_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    )];
    let input = RouteInput {
        task: TaskKind::CodeRepair,
        objective: Objective::Fastest,
        min_confidence: Some("low".to_owned()),
        ..RouteInput::default()
    };
    let decision = select_route(&input, &models, &SessionAffinity::default()).unwrap();
    assert_eq!(decision.confidence, "low");
}

/// A task refused for lack of an admission slot must not have mutated session affinity.
///
/// Binding at routing time meant a 503'd request had already pinned the session to a model: the
/// caller saw a refusal and reasonably concluded nothing happened, while every later request in
/// that session had silently been redirected. State changes belong after the last thing that can
/// refuse.
#[tokio::test]
async fn a_refused_task_does_not_bind_session_affinity() {
    let mock = discovery_routes::<()>()
        .route(
            "/api/embed",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Json(json!({"embeddings": [[0.1]]}))
            }),
        )
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform =
        app(
            &PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
                .with_max_concurrent_tasks(1)
                .with_max_queue_wait(Duration::from_millis(50)),
        )
        .unwrap();

    // Open a real session.
    let created = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/sessions")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let created: Value =
        serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap()).unwrap();
    let session = created["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let bound_task = |session: String| {
        Request::post("/_freellama/v1/tasks")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"task":"embedding","objective":"fastest","model":"embed-model","input":"x","session_id":"{session}"}}"#
            )))
            .unwrap()
    };

    // Saturate the single slot, then let a second request be refused.
    let holder = {
        let p = platform.clone();
        let s = session.clone();
        tokio::spawn(async move { p.oneshot(bound_task(s)).await.unwrap().status() })
    };
    tokio::time::sleep(Duration::from_millis(80)).await;
    // A DIFFERENT model, so a stray bind would be visible.
    let refused = platform
        .clone()
        .oneshot(
            Request::post("/_freellama/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"task":"embedding","objective":"fastest","model":"embed-model","input":"y","session_id":"{session}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(holder.await.unwrap(), StatusCode::OK);
    mock_task.abort();
}

/// `confidence` must be derivable from the dimensions, not a standalone opaque grade.
///
/// A single word invites being read as a calibrated probability. It is not one: `medium` means "a
/// policy vouched for this model on this task" AND "a functional benchmark exists" — two different
/// claims about two different kinds of evidence. Reporting them separately is what makes the router
/// inspectable, and lets a caller disagree with the collapse rather than only with the verdict.
#[test]
fn routing_reports_its_evidence_dimensions_separately() {
    let models = vec![candidate(
        "qwen3.8:27b-mlx",
        18_000_000_000,
        &[Capability::Completion, Capability::Tools],
        false,
    )];
    let decision = select_route(
        &RouteInput {
            task: TaskKind::CodeRepair,
            objective: Objective::Fastest,
            ..RouteInput::default()
        },
        &models,
        &SessionAffinity::default(),
    )
    .unwrap();

    // Every dimension is reported, and none is silently absent.
    for (name, value) in [
        ("quality_evidence", &decision.quality_evidence),
        ("task_evidence", &decision.task_evidence),
        ("hardware_fit", &decision.hardware_fit),
    ] {
        assert!(!value.is_empty(), "{name} must be reported, got empty");
    }
    // `medium` requires BOTH quality and task evidence; anything less must not claim it.
    if decision.confidence == "medium" {
        assert_eq!(decision.quality_evidence, "strong");
        assert_eq!(decision.task_evidence, "strong");
    } else {
        assert!(
            decision.quality_evidence != "strong" || decision.task_evidence != "strong",
            "confidence should have been medium given both dimensions strong"
        );
    }
}

/// A losing candidate must come back with a reason. Naming only the winner makes the comparison
/// unauditable — a caller cannot tell a considered rejection from a model that was never seen.
#[test]
fn rejected_candidates_are_reported_with_a_reason() {
    let models = vec![
        candidate(
            "big",
            18_000_000_000,
            &[Capability::Completion, Capability::Tools],
            true,
        ),
        candidate(
            "small",
            900_000_000,
            &[Capability::Completion, Capability::Tools],
            false,
        ),
    ];
    let decision = select_route(
        &RouteInput {
            task: TaskKind::CodeRepair,
            objective: Objective::Fastest,
            ..RouteInput::default()
        },
        &models,
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(
        decision.rejected.len(),
        1,
        "one candidate lost, it must be listed"
    );
    let loser = &decision.rejected[0];
    assert_ne!(loser["model"].as_str().unwrap(), decision.selected_model);
    assert!(
        loser["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "a rejection without a reason is not inspectable, got {loser}"
    );
}

/// `/health` must carry the load-shedding signal. An agent deciding "delegate, queue, or do it
/// myself" needs a cheap read-only answer to "will a task be admitted right now?" — without it the
/// only way to find out is to submit and possibly eat the full queue wait or a 503.
#[tokio::test]
async fn health_reports_admission_capacity() {
    let mock = discovery_routes::<()>().with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let platform =
        app(
            &PlatformConfig::new("127.0.0.1:11435", upstream, None, None, "qwen2.5:0.5b")
                .with_max_concurrent_tasks(5),
        )
        .unwrap();

    let response = platform
        .oneshot(
            Request::get("/_freellama/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["admission"]["slots_total"], 5);
    assert_eq!(
        body["admission"]["slots_available"], 5,
        "idle server must report full capacity"
    );
    assert!(
        body["admission"]["max_queue_wait_seconds"]
            .as_u64()
            .is_some()
    );
    assert_eq!(body["admission"]["costs"]["vision"], 4);
    mock_task.abort();
}
