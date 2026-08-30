use freellama::model_bench::{
    BenchmarkConfiguration, Capability, ModelMetadata, benchmark_plan, score_cases,
};

fn model(capabilities: &[Capability], size: u64) -> ModelMetadata {
    ModelMetadata {
        name: "fixture:latest".to_owned(),
        size,
        format: "gguf".to_owned(),
        family: "fixture".to_owned(),
        parameter_size: "1B".to_owned(),
        quantization: "Q4".to_owned(),
        capabilities: capabilities.to_vec(),
        advertised_context: Some(32_768),
    }
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

#[test]
fn score_is_quality_guarded() {
    assert!((score_cases(3, 4, 2_000) - 5_400.0).abs() < f64::EPSILON);
    assert!(score_cases(0, 4, 2_000).abs() < f64::EPSILON);
}

#[test]
fn large_models_receive_a_memory_safe_mac_profile() {
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
