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
