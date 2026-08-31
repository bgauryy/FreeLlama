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
