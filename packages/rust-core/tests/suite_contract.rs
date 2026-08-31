use freellama::{
    Comparison, GuardrailStatus, RunReport, Suite, Verdict, compare, max_loaded_models_advisory,
    parse_ollama_cli_version,
};

#[test]
fn doctor_identifies_a_cli_server_version_mismatch() {
    let diagnostic = parse_ollama_cli_version(
        "0.32.15",
        "ollama version is 0.32.15\n",
        "Warning: client version is 0.13.5\n",
    );

    assert_eq!(diagnostic.client_version.as_deref(), Some("0.13.5"));
    assert_eq!(diagnostic.server_version, "0.32.15");
    assert!(!diagnostic.matches_server);
}

#[test]
fn max_loaded_models_advisory_warns_when_unset() {
    assert!(max_loaded_models_advisory(None).is_some());
    assert!(max_loaded_models_advisory(Some("")).is_some());
}

#[test]
fn max_loaded_models_advisory_silent_when_configured() {
    assert!(max_loaded_models_advisory(Some("1")).is_none());
    assert!(max_loaded_models_advisory(Some("3")).is_none());
}

#[test]
fn checked_in_suite_expands_every_upstream_regression() {
    let suite = Suite::from_path(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../benchmark/suites/ollama-mlx-regressions.json"
    ))
    .unwrap();
    let cases = suite.expand().unwrap();

    assert!(
        cases
            .iter()
            .any(|case| case.id.starts_with("prefix-reuse/"))
    );
    assert!(
        cases
            .iter()
            .any(|case| case.id.starts_with("cache-restore/"))
    );
    assert!(
        cases
            .iter()
            .any(|case| case.id.starts_with("cache-growth/"))
    );
    assert!(
        cases
            .iter()
            .any(|case| case.id.starts_with("runner-reload/"))
    );
    assert!(
        cases
            .iter()
            .any(|case| case.id.starts_with("model-transition/"))
    );
}

#[test]
fn comparison_rejects_output_drift_even_when_candidate_is_faster() {
    let baseline = RunReport::fixture(10_000, "same");
    let candidate = RunReport::fixture(5_000, "changed");

    let Comparison { verdict, .. } = compare(&baseline, &candidate, 0.20).unwrap();
    assert_eq!(verdict, Verdict::Reject);
}

#[test]
fn comparison_accepts_a_faster_exact_candidate() {
    let baseline = RunReport::fixture(10_000, "same");
    let candidate = RunReport::fixture(7_000, "same");

    let Comparison { verdict, .. } = compare(&baseline, &candidate, 0.20).unwrap();
    assert_eq!(verdict, Verdict::Accept);
}

#[test]
fn comparison_rejects_a_faster_candidate_that_grows_resident_memory() {
    let mut baseline = RunReport::fixture(10_000, "same");
    baseline.cases[0].resident_size = Some(10_000);
    let mut candidate = RunReport::fixture(7_000, "same");
    candidate.cases[0].resident_size = Some(20_000);

    let comparison = compare(&baseline, &candidate, 0.20).unwrap();
    assert_eq!(comparison.guardrails.memory, GuardrailStatus::Fail);
    assert_eq!(comparison.verdict, Verdict::Reject);
}
