use freellama::capability::{Capability, ModelType};
use freellama::model_bench::{BenchmarkConfiguration, ModelMetadata, benchmark_plan, score_cases};

fn model(capabilities: &[Capability], size: u64) -> ModelMetadata {
    ModelMetadata {
        name: "fixture:latest".to_owned(),
        digest: None,
        size,
        format: "gguf".to_owned(),
        family: "fixture".to_owned(),
        parameter_size: "1B".to_owned(),
        quantization: "Q4".to_owned(),
        capabilities: capabilities.to_vec(),
        model_type: ModelType::from_capabilities(capabilities.iter().copied()),
        advertised_context: Some(32_768),
    }
}

#[test]
fn model_type_is_derived_from_additive_capabilities() {
    assert_eq!(
        ModelType::from_capabilities([Capability::Embedding]),
        ModelType::EmbeddingOnly
    );
    assert_eq!(
        ModelType::from_capabilities([Capability::Completion, Capability::Vision]),
        ModelType::Multimodal
    );
    assert_eq!(
        ModelType::from_capabilities([Capability::Completion, Capability::Tools]),
        ModelType::Generative
    );
}

#[test]
fn benchmark_selects_cases_by_capability() {
    let cases = benchmark_plan(&model(
        &[
            Capability::Completion,
            Capability::Tools,
            Capability::Vision,
        ],
        8_000_000_000,
    ));
    assert!(cases.iter().any(|case| case.id == "text/exact"));
    assert!(cases.iter().any(|case| case.id == "text/long-needle"));
    assert!(cases.iter().any(|case| case.id == "tools/multiply"));
    assert!(cases.iter().any(|case| case.id == "tools/recovery"));
    assert!(cases.iter().any(|case| case.id == "vision/color"));
    assert!(!cases.iter().any(|case| case.id == "embedding/similarity"));
}

/// An embedding model can advertise `vision` and `audio` (its encoder reads them) without
/// `completion`; `/api/chat` refuses it with 400, so it must get no chat-shaped case.
#[test]
fn chat_cases_require_completion_even_when_vision_or_tools_are_advertised() {
    let embedding = benchmark_plan(&model(
        &[Capability::Vision, Capability::Audio, Capability::Embedding],
        800_000_000,
    ));
    let ids = embedding.iter().map(|case| case.id).collect::<Vec<_>>();
    assert_eq!(ids, ["embedding/integrity"]);

    let tools_only = benchmark_plan(&model(&[Capability::Tools, Capability::Vision], 1_000));
    assert!(tools_only.is_empty(), "{tools_only:?}");
}

#[test]
fn decision_models_get_a_decision_case_and_no_chat_case() {
    let plan = benchmark_plan(&model(&[Capability::Decision], 2_000_000_000));
    let ids = plan.iter().map(|case| case.id).collect::<Vec<_>>();
    assert_eq!(ids, ["decision/noul"]);
    assert_eq!(plan[0].capability, Capability::Decision);
}

#[test]
fn speech_only_models_remain_uncovered() {
    assert!(benchmark_plan(&model(&[Capability::Speech], 1_000)).is_empty());
}

#[test]
fn score_is_quality_guarded() {
    assert!((score_cases(3, 2_000) - 5_400.0).abs() < f64::EPSILON);
    assert!(score_cases(0, 2_000).abs() < f64::EPSILON);
}

#[test]
fn large_models_receive_a_conservative_cross_machine_profile() {
    let plan = benchmark_plan(&model(&[Capability::Completion], 20_000_000_000));
    assert!(plan.iter().all(|case| case.num_ctx <= 8_192));
}

#[test]
fn benchmark_configuration_discloses_reproducible_defaults() {
    let configuration = BenchmarkConfiguration::default();
    assert!(configuration.temperature.abs() < f64::EPSILON);
    assert_eq!(configuration.seed, 42);
    assert!(!configuration.think);
    assert_eq!(configuration.num_predict, 128);
    assert_eq!(configuration.cache_token_metrics, "not_reported_by_ollama");
}

#[derive(Clone)]
struct BenchServer {
    chats: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    unloads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    resident_at_start: bool,
    fail_chat: bool,
    replace_after_chat: bool,
}

async fn bench_fixture(
    resident_at_start: bool,
    fail_chat: bool,
    replace_after_chat: bool,
) -> (String, BenchServer, tokio::task::JoinHandle<()>) {
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use std::sync::atomic::Ordering;
    let state = BenchServer {
        chats: std::sync::Arc::default(),
        unloads: std::sync::Arc::default(),
        resident_at_start,
        fail_chat,
        replace_after_chat,
    };
    let app = Router::new()
        .route("/api/version", get(|| async { Json(json!({"version":"fixture"})) }))
        .route("/api/tags", get(|State(state): State<BenchServer>| async move {
            let digest = if state.replace_after_chat && state.chats.load(Ordering::SeqCst) > 0 { "replaced" } else { "measured" };
            Json(json!({"models":[{"name":"fixture:1b","digest":digest,"size":1024}]}))
        }))
        .route("/api/show", post(|| async { Json(json!({"capabilities":["completion"],"model_info":{"fixture.context_length":4096}})) }))
        .route("/api/ps", get(|State(state): State<BenchServer>| async move {
            let resident = state.resident_at_start || state.chats.load(Ordering::SeqCst) > 0;
            Json(json!({"models":if resident { vec![json!({"name":"fixture:1b","digest":"measured","size":2048,"size_vram":0})] } else { vec![] }}))
        }))
        .route("/api/chat", post(|State(state): State<BenchServer>, Json(body): Json<Value>| async move {
            state.chats.fetch_add(1, Ordering::SeqCst);
            if state.fail_chat { return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"did not execute; another client loaded this model"}))); }
            let prompt = body["messages"][0]["content"].as_str().unwrap_or("");
            let answer = if prompt.contains("FREELLAMA_OK") { "FREELLAMA_OK" } else if prompt.contains("17 * 23") { "391" } else { "ALPINE-4721" };
            (StatusCode::OK, Json(json!({"done":true,"message":{"role":"assistant","content":answer}})))
        }))
        .route("/api/generate", post(|State(state): State<BenchServer>| async move {
            state.unloads.fetch_add(1, Ordering::SeqCst);
            Json(json!({"done":true}))
        })).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, state, task)
}

#[tokio::test]
async fn benchmark_preserves_existing_runners_and_only_cleans_up_successful_warming() {
    use freellama::model_bench::{BenchConfig, benchmark_all};
    use std::sync::atomic::Ordering;
    for (resident, fails, replaced, expected_unloads) in [
        (true, false, false, 0),
        (false, false, false, 1),
        (false, true, false, 0),
        (false, false, true, 0),
    ] {
        let (endpoint, state, server) = bench_fixture(resident, fails, replaced).await;
        let report = benchmark_all(&BenchConfig {
            endpoint,
            include: vec!["fixture:1b".into()],
            timeout_seconds: 5,
            trials: 1,
        })
        .await
        .unwrap();
        assert_eq!(report.schema_version, 2);
        assert_eq!(report.models[0].cold_start.is_some(), !resident);
        assert_eq!(state.unloads.load(Ordering::SeqCst), expected_unloads);
        assert_eq!(report.models[0].cases.len(), 3);
        server.abort();
    }
}

#[tokio::test]
async fn benchmark_unknown_exact_tag_refuses_before_any_generation() {
    use freellama::model_bench::{BenchConfig, benchmark_all};
    use std::sync::atomic::Ordering;
    let (endpoint, state, server) = bench_fixture(false, false, false).await;
    let result = benchmark_all(&BenchConfig {
        endpoint,
        include: vec!["fixture:1b".into(), "missing:1b".into()],
        timeout_seconds: 5,
        trials: 1,
    })
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("exact installed tag")
    );
    assert_eq!(state.chats.load(Ordering::SeqCst), 0);
    assert_eq!(state.unloads.load(Ordering::SeqCst), 0);
    server.abort();
}
