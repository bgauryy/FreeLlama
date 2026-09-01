//! Adversarial edge cases the per-module contract tests do not pin down: boundary arithmetic,
//! tie-breaking stability, hostile/malformed inputs, fail-closed defaults, and fixed regressions.

use std::collections::BTreeMap;
use std::io::Write;

use freellama::model_bench::{Capability, ModelMetadata, ModelType, benchmark_plan};
use freellama::platform::{
    CatalogModel, Objective, PlatformConfig, RouteInput, RouteIntent, SessionAffinity, TaskKind,
    normalize_route_intent, runtime_metrics, select_route,
};
use freellama::policy::qualify_from_aggregate;
use freellama::proxy::{ProxyConfig, proxy_target};
use freellama::recommend::{
    FitStatus, InstallationPlanRequest, RecommendationCatalog, installation_plans,
};
use freellama::{GuardrailStatus, RunReport, Suite, Verdict, compare, parse_ollama_cli_version};
use serde_json::json;

fn model(name: &str, size: u64, capabilities: &[Capability]) -> CatalogModel {
    CatalogModel {
        name: name.to_owned(),
        size,
        capabilities: capabilities.iter().copied().collect(),
        advertised_context: Some(32_768),
        resident: false,
        resident_vram: None,
        benchmark: BTreeMap::new(),
        policy_rank: BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Routing: ordering stability and boundary conditions
// ---------------------------------------------------------------------------

/// Two indistinguishable candidates (same size, no benchmark) must resolve to the same winner no
/// matter what order discovery returned them in — an order-dependent router would flap between
/// models across catalog refreshes, silently thrashing residency.
#[test]
fn fastest_tie_break_is_deterministic_regardless_of_catalog_order() {
    let alpha = model("alpha:1b", 1_000_000_000, &[Capability::Completion]);
    let beta = model("beta:1b", 1_000_000_000, &[Capability::Completion]);
    let input = RouteInput {
        objective: Objective::Fastest,
        ..RouteInput::default()
    };

    let forward = select_route(
        &input,
        &[alpha.clone(), beta.clone()],
        &SessionAffinity::default(),
    )
    .unwrap();
    let reversed = select_route(&input, &[beta, alpha], &SessionAffinity::default()).unwrap();

    assert_eq!(forward.selected_model, reversed.selected_model);
    assert_eq!(forward.selected_model, "alpha:1b");
}

#[test]
fn fastest_prefers_the_smaller_model_when_benchmark_scores_tie() {
    let mut small = model("small:1b", 1_000_000_000, &[Capability::Completion]);
    small.benchmark.insert(Capability::Completion, 100.0);
    let mut large = model("large:8b", 8_000_000_000, &[Capability::Completion]);
    large.benchmark.insert(Capability::Completion, 100.0);

    let decision = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            ..RouteInput::default()
        },
        &[large, small],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(decision.selected_model, "small:1b");
}

/// Balanced routing prefers an already-resident model over a colder one with a higher benchmark
/// score. Pinned deliberately: if this ordering ever changes, warm-model reuse (the main latency
/// win on unified memory) silently disappears.
#[test]
fn balanced_prefers_a_resident_model_over_a_faster_cold_one() {
    let mut hot = model("hot:7b", 7_000_000_000, &[Capability::Completion]);
    hot.resident = true;
    hot.benchmark.insert(Capability::Completion, 10.0);
    hot.policy_rank.insert(TaskKind::Completion, 0);
    let mut cold = model("cold:7b", 7_000_000_000, &[Capability::Completion]);
    cold.benchmark.insert(Capability::Completion, 10_000.0);
    cold.policy_rank.insert(TaskKind::Completion, 0);

    let decision = select_route(
        &RouteInput::default(),
        &[cold, hot],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(decision.selected_model, "hot:7b");
    assert!(decision.reasons.iter().any(|reason| reason == "resident"));
}

/// A session pinned to a model that has since been uninstalled must fall back to normal selection,
/// not error and not report the affinity it could not honor.
#[test]
fn session_affinity_to_an_uninstalled_model_falls_back_to_normal_selection() {
    let stay = model("stay:1b", 1_000_000_000, &[Capability::Completion]);
    let sessions = SessionAffinity::from_pairs([("session-a".to_owned(), "gone:1b".to_owned())]);

    let decision = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            session_id: Some("session-a".to_owned()),
            ..RouteInput::default()
        },
        &[stay],
        &sessions,
    )
    .unwrap();

    assert_eq!(decision.selected_model, "stay:1b");
    assert!(
        !decision
            .reasons
            .iter()
            .any(|reason| reason == "session_affinity"),
        "an affinity that could not be honored must not be reported as the reason: {:?}",
        decision.reasons
    );
}

/// A caller asking for a specific window against a model whose window is unknown must be refused,
/// not optimistically admitted — `advertised_context: None` means "cannot verify", not "infinite".
#[test]
fn context_request_fails_closed_when_the_advertised_window_is_unknown() {
    let mut unknown = model("mystery:7b", 7_000_000_000, &[Capability::Completion]);
    unknown.advertised_context = None;

    let result = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            context_tokens: Some(1_024),
            ..RouteInput::default()
        },
        &[unknown],
        &SessionAffinity::default(),
    );

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no installed model is eligible")
    );
}

/// The window comparison is inclusive: a request for exactly the advertised window fits; one token
/// more does not.
#[test]
fn context_eligibility_boundary_is_inclusive_of_the_advertised_window() {
    let mut windowed = model("windowed:7b", 7_000_000_000, &[Capability::Completion]);
    windowed.advertised_context = Some(8_192);
    let at_limit = RouteInput {
        objective: Objective::Fastest,
        context_tokens: Some(8_192),
        ..RouteInput::default()
    };
    let over_limit = RouteInput {
        context_tokens: Some(8_193),
        ..at_limit.clone()
    };

    let decision =
        select_route(&at_limit, &[windowed.clone()], &SessionAffinity::default()).unwrap();
    assert_eq!(decision.options["num_ctx"], 8_192);

    assert!(
        select_route(&over_limit, &[windowed], &SessionAffinity::default()).is_err(),
        "8193 tokens against an 8192 window must be refused"
    );
}

/// `num_ctx` is sent to Ollama as an option that ultimately lands in a u32; a u64-sized request
/// must clamp rather than wrap or panic.
#[test]
fn a_u64_sized_context_request_clamps_to_u32_max() {
    let mut vast = model("vast:7b", 7_000_000_000, &[Capability::Completion]);
    vast.advertised_context = Some(u64::MAX);

    let decision = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            context_tokens: Some(u64::MAX),
            ..RouteInput::default()
        },
        &[vast],
        &SessionAffinity::default(),
    )
    .unwrap();

    assert_eq!(decision.options["num_ctx"], u64::from(u32::MAX));
}

/// The gate matches grades exactly. "Medium" (capitalized) is not a grade this router issues, and
/// treating it as one — or ranking it lowest — would silently disable the fail-closed floor.
#[test]
fn min_confidence_grades_are_case_sensitive_and_fail_closed() {
    let candidate = model("plain:7b", 7_000_000_000, &[Capability::Completion]);

    let error = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            min_confidence: Some("Medium".to_owned()),
            ..RouteInput::default()
        },
        &[candidate],
        &SessionAffinity::default(),
    )
    .expect_err("an unrecognized grade must refuse, never rank lowest");

    assert!(error.to_string().contains("unknown min_confidence"));
}

/// Naming an explicit model does not raise the evidence grade — the grade measures the evidence,
/// not who chose. The confidence floor must therefore still refuse an explicit low-evidence pick.
#[test]
fn an_explicit_model_does_not_bypass_the_confidence_gate() {
    let candidate = model("plain:7b", 7_000_000_000, &[Capability::Completion]);

    let error = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            model: Some("plain:7b".to_owned()),
            min_confidence: Some("medium".to_owned()),
            ..RouteInput::default()
        },
        &[candidate],
        &SessionAffinity::default(),
    )
    .expect_err("explicit model selection must not launder a low grade into acceptance");

    assert!(error.to_string().contains("route refused"));
}

/// `medium` is a conjunction: policy AND benchmark. Either alone must stay `low`.
#[test]
fn medium_confidence_requires_both_policy_and_benchmark_evidence() {
    let mut both = model("both:7b", 7_000_000_000, &[Capability::Completion]);
    both.policy_rank.insert(TaskKind::Completion, 0);
    both.benchmark.insert(Capability::Completion, 42.0);
    let decision =
        select_route(&RouteInput::default(), &[both], &SessionAffinity::default()).unwrap();
    assert_eq!(decision.confidence, "medium");
    assert_eq!(decision.quality_evidence, "strong");
    assert_eq!(decision.task_evidence, "strong");

    let mut policy_only = model("policy:7b", 7_000_000_000, &[Capability::Completion]);
    policy_only.policy_rank.insert(TaskKind::Completion, 0);
    let decision = select_route(
        &RouteInput::default(),
        &[policy_only],
        &SessionAffinity::default(),
    )
    .unwrap();
    assert_eq!(decision.confidence, "low");
    assert_eq!(decision.task_evidence, "none");
}

/// "Not installed" and "installed but not eligible" are different caller mistakes and must stay
/// distinguishable in the error.
#[test]
fn an_explicit_model_that_is_not_installed_says_so() {
    let candidate = model("present:7b", 7_000_000_000, &[Capability::Completion]);

    let error = select_route(
        &RouteInput {
            objective: Objective::Fastest,
            model: Some("ghost:7b".to_owned()),
            ..RouteInput::default()
        },
        &[candidate],
        &SessionAffinity::default(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("not installed"));
}

// ---------------------------------------------------------------------------
// KPI comparison: guardrail boundaries and fail-closed hash handling
// ---------------------------------------------------------------------------

/// The memory guardrail allows exactly +5% (`stock + stock/20`), inclusive. One byte past the
/// allowance must fail — the whole point of a guardrail is that the edge is sharp.
#[test]
fn memory_guardrail_boundary_is_exactly_five_percent_inclusive() {
    let mut baseline = RunReport::fixture(10_000, "same");
    baseline.cases[0].resident_size = Some(20_000);

    let mut at_allowance = RunReport::fixture(7_000, "same");
    at_allowance.cases[0].resident_size = Some(21_000);
    let comparison = compare(&baseline, &at_allowance, 0.20).unwrap();
    assert_eq!(comparison.guardrails.memory, GuardrailStatus::Pass);

    let mut past_allowance = RunReport::fixture(7_000, "same");
    past_allowance.cases[0].resident_size = Some(21_001);
    let comparison = compare(&baseline, &past_allowance, 0.20).unwrap();
    assert_eq!(comparison.guardrails.memory, GuardrailStatus::Fail);
}

/// A candidate that stopped reporting resident memory is not "no regression" — it is missing the
/// evidence the guardrail needs, and must fail rather than pass by omission.
#[test]
fn memory_guardrail_fails_when_only_one_side_reports_residency() {
    let mut baseline = RunReport::fixture(10_000, "same");
    baseline.cases[0].resident_size = Some(20_000);
    let candidate = RunReport::fixture(7_000, "same");

    let comparison = compare(&baseline, &candidate, 0.20).unwrap();
    assert_eq!(comparison.guardrails.memory, GuardrailStatus::Fail);
    assert_eq!(comparison.verdict, Verdict::Reject);
}

/// Two errored runs have no output hashes on either side. "None == None" must not count as exact
/// output equivalence — absent evidence is not agreement.
#[test]
fn missing_output_hashes_fail_the_exact_outputs_guardrail() {
    let mut baseline = RunReport::fixture(10_000, "same");
    baseline.cases[0].output_hash = None;
    let mut candidate = RunReport::fixture(7_000, "same");
    candidate.cases[0].output_hash = None;

    let comparison = compare(&baseline, &candidate, 0.20).unwrap();
    assert_eq!(comparison.guardrails.exact_outputs, GuardrailStatus::Fail);
    assert_eq!(comparison.verdict, Verdict::Reject);
}

#[test]
fn comparison_refuses_reports_with_different_case_counts() {
    let baseline = RunReport::fixture(10_000, "same");
    let mut candidate = RunReport::fixture(7_000, "same");
    let mut extra = candidate.cases[0].clone();
    extra.id = "case-2".to_owned();
    candidate.cases.push(extra);

    let error = compare(&baseline, &candidate, 0.20).unwrap_err();
    assert!(error.to_string().contains("case counts differ"));
}

// ---------------------------------------------------------------------------
// Suite loading: hostile and degenerate files
// ---------------------------------------------------------------------------

fn suite_file(contents: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file
}

const SUITE_DEFAULTS: &str = r#""defaults":{"seed":1,"temperature":0.0,"num_predict":8,"num_ctx":512,"timeout_seconds":5,"target_improvement":0.1}"#;

#[test]
fn suite_loading_rejects_malformed_and_degenerate_files() {
    let not_json = suite_file("{ this is not json");
    assert!(Suite::from_path(not_json.path()).is_err());

    let wrong_version = suite_file(&format!(
        r#"{{"schema_version":2,"name":"s",{SUITE_DEFAULTS},"scenarios":[{{"kind":"runner_reload","id":"a","prompt":"p","repetitions":1}}]}}"#
    ));
    let error = Suite::from_path(wrong_version.path()).unwrap_err();
    assert!(error.to_string().contains("unsupported suite schema"));

    let no_scenarios = suite_file(&format!(
        r#"{{"schema_version":1,"name":"s",{SUITE_DEFAULTS},"scenarios":[]}}"#
    ));
    let error = Suite::from_path(no_scenarios.path()).unwrap_err();
    assert!(error.to_string().contains("no scenarios"));

    // deny_unknown_fields: a misspelled key must fail parsing, not be silently dropped.
    let unknown_field = suite_file(&format!(
        r#"{{"schema_version":1,"name":"s","scenarois":[],{SUITE_DEFAULTS},"scenarios":[{{"kind":"runner_reload","id":"a","prompt":"p","repetitions":1}}]}}"#
    ));
    assert!(Suite::from_path(unknown_field.path()).is_err());
}

/// Duplicate case ids would make equivalence-group and per-case comparisons silently ambiguous.
#[test]
fn suite_expansion_refuses_scenarios_that_collide_on_id() {
    let colliding = suite_file(&format!(
        r#"{{"schema_version":1,"name":"s",{SUITE_DEFAULTS},"scenarios":[
            {{"kind":"runner_reload","id":"dup","prompt":"p","repetitions":1}},
            {{"kind":"runner_reload","id":"dup","prompt":"q","repetitions":1}}
        ]}}"#
    ));
    let suite = Suite::from_path(colliding.path()).unwrap();
    let error = suite.expand().unwrap_err();
    assert!(error.to_string().contains("duplicate case id"));
}

#[test]
fn suite_expansion_refuses_zero_repetition_scenarios() {
    let zero_reload = suite_file(&format!(
        r#"{{"schema_version":1,"name":"s",{SUITE_DEFAULTS},"scenarios":[
            {{"kind":"runner_reload","id":"r","prompt":"p","repetitions":0}}
        ]}}"#
    ));
    let error = Suite::from_path(zero_reload.path())
        .unwrap()
        .expand()
        .unwrap_err();
    assert!(error.to_string().contains("repetitions is zero"));

    let zero_prefix = suite_file(&format!(
        r#"{{"schema_version":1,"name":"s",{SUITE_DEFAULTS},"scenarios":[
            {{"kind":"prefix_reuse","id":"p","prefix":"x","prefix_repetitions":0,"turns":["t"]}}
        ]}}"#
    ));
    let error = Suite::from_path(zero_prefix.path())
        .unwrap()
        .expand()
        .unwrap_err();
    assert!(error.to_string().contains("prefix_repetitions is zero"));
}

// ---------------------------------------------------------------------------
// Doctor version parsing
// ---------------------------------------------------------------------------

/// A CLI that produced no output must not read as "matches the server" — `None` on both sides is
/// absence of evidence, not agreement.
#[test]
fn empty_cli_output_never_claims_a_version_match() {
    let diagnostic = parse_ollama_cli_version("0.12.6", "", "");
    assert_eq!(diagnostic.client_version, None);
    assert_eq!(diagnostic.reported_version, None);
    assert!(!diagnostic.matches_server);
    assert_eq!(diagnostic.warning, None);

    let matching = parse_ollama_cli_version("0.12.6", "ollama version is 0.12.6\n", "");
    assert!(matching.matches_server);
    assert_eq!(matching.warning, None);
}

// ---------------------------------------------------------------------------
// Benchmark planning: size tiers and context clamping
// ---------------------------------------------------------------------------

fn metadata(size: u64, advertised_context: Option<u64>) -> ModelMetadata {
    ModelMetadata {
        name: "fixture:latest".to_owned(),
        size,
        format: "gguf".to_owned(),
        family: "fixture".to_owned(),
        parameter_size: "1B".to_owned(),
        quantization: "Q4".to_owned(),
        capabilities: vec![Capability::Completion],
        model_type: ModelType::Generative,
        advertised_context,
    }
}

#[test]
fn benchmark_size_tier_boundaries_are_inclusive_lower_bounds() {
    let ctx = |size| benchmark_plan(&metadata(size, Some(1_000_000)))[0].num_ctx;
    assert_eq!(ctx(16_000_000_000), 8_192, "exactly 16GB is the large tier");
    assert_eq!(ctx(15_999_999_999), 16_384, "one byte under 16GB is medium");
    assert_eq!(ctx(2_000_000_000), 16_384, "exactly 2GB is the medium tier");
    assert_eq!(ctx(1_999_999_999), 32_768, "one byte under 2GB is small");
}

/// Models advertising absurd context windows (larger than u32) must fall back to the memory-safe
/// tier rather than panicking or wrapping in the u64 -> u32 conversion.
#[test]
fn benchmark_plan_survives_a_context_window_larger_than_u32() {
    let plan = benchmark_plan(&metadata(1_000_000_000, Some(u64::MAX)));
    assert_eq!(plan[0].num_ctx, 32_768);

    let clamped = benchmark_plan(&metadata(1_000_000_000, Some(1_024)));
    assert_eq!(
        clamped[0].num_ctx, 1_024,
        "a small real window still clamps"
    );
}

// ---------------------------------------------------------------------------
// Recommendations: fit boundaries, filtering, date validation
// ---------------------------------------------------------------------------

fn catalog_from(text: &str) -> RecommendationCatalog {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recommendations.toml");
    std::fs::write(&path, text).unwrap();
    RecommendationCatalog::from_path(&path).unwrap()
}

fn single_model_catalog() -> RecommendationCatalog {
    catalog_from(
        r#"schema_version = 1
reviewed_at = "2026-08-24"
review_due_at = "2026-09-23"

[[models]]
name = "reviewed:latest"
summary = "Reviewed completion model."
tasks = ["completion"]
capabilities = ["completion"]
max_context_tokens = 8192
estimated_download_bytes = 1000
minimum_memory_bytes = 2000
"#,
    )
}

fn plan_request<'a>(
    installed: &'a std::collections::BTreeSet<String>,
    required: &'a std::collections::BTreeSet<Capability>,
    memory_bytes: Option<u64>,
) -> InstallationPlanRequest<'a> {
    InstallationPlanRequest {
        task: TaskKind::Completion,
        explicit_model: None,
        required_capabilities: required,
        requested_context: 4_096,
        installed_models: installed,
        memory_bytes,
        available_disk_bytes: Some(1_000_000),
    }
}

/// Fit is inclusive at exactly the declared requirement; one byte less does not fit; and an
/// unmeasurable machine reports Unknown with a warning rather than guessing either way.
#[test]
fn memory_fit_boundary_is_inclusive_and_unknown_when_unmeasured() {
    let catalog = single_model_catalog();
    let installed = std::collections::BTreeSet::new();
    let required = [Capability::Completion].into_iter().collect();

    let exact = installation_plans(&catalog, &plan_request(&installed, &required, Some(2_000)));
    assert_eq!(exact[0].memory_fit, FitStatus::Fits);
    assert!(exact[0].warnings.is_empty());

    let short = installation_plans(&catalog, &plan_request(&installed, &required, Some(1_999)));
    assert_eq!(short[0].memory_fit, FitStatus::DoesNotFit);
    assert!(short[0].warnings.iter().any(|w| w.contains("host memory")));

    let unknown = installation_plans(&catalog, &plan_request(&installed, &required, None));
    assert_eq!(unknown[0].memory_fit, FitStatus::Unknown);
    assert!(unknown[0].warnings.iter().any(|w| w.contains("verify")));
}

#[test]
fn an_already_installed_model_is_never_planned_for_installation() {
    let catalog = single_model_catalog();
    let installed = ["reviewed:latest".to_owned()].into_iter().collect();
    let required = [Capability::Completion].into_iter().collect();

    let plans = installation_plans(&catalog, &plan_request(&installed, &required, Some(2_000)));
    assert!(plans.is_empty());
}

/// A model that fits must outrank one that does not, even when the non-fitting model has the
/// stronger (lower) priority — recommending an install that cannot run is worse than recommending
/// a second choice that can.
#[test]
fn fitting_models_outrank_higher_priority_models_that_do_not_fit() {
    let catalog = catalog_from(
        r#"schema_version = 1
reviewed_at = "2026-08-24"
review_due_at = "2026-09-23"

[[models]]
name = "huge:70b"
summary = "Top pick, does not fit."
tasks = ["completion"]
capabilities = ["completion"]
max_context_tokens = 8192
estimated_download_bytes = 1000
minimum_memory_bytes = 100000
priority = 1

[[models]]
name = "small:1b"
summary = "Second pick, fits."
tasks = ["completion"]
capabilities = ["completion"]
max_context_tokens = 8192
estimated_download_bytes = 1000
minimum_memory_bytes = 1000
priority = 100
"#,
    );
    let installed = std::collections::BTreeSet::new();
    let required = [Capability::Completion].into_iter().collect();

    let plans = installation_plans(&catalog, &plan_request(&installed, &required, Some(2_000)));
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].model, "small:1b");
}

#[test]
fn catalog_date_validation_understands_the_calendar() {
    let with_dates = |reviewed: &str, due: &str| {
        format!(
            r#"schema_version = 1
reviewed_at = "{reviewed}"
review_due_at = "{due}"

[[models]]
name = "reviewed:latest"
summary = "Reviewed completion model."
tasks = ["completion"]
capabilities = ["completion"]
max_context_tokens = 8192
estimated_download_bytes = 1000
minimum_memory_bytes = 2000
"#
        )
    };
    let load = |reviewed: &str, due: &str| {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recommendations.toml");
        std::fs::write(&path, with_dates(reviewed, due)).unwrap();
        RecommendationCatalog::from_path(&path).map(|_| ())
    };

    assert!(
        load("2000-02-29", "2000-03-01").is_ok(),
        "2000 is a leap year (400 rule)"
    );
    assert!(
        load("2100-02-29", "2100-03-01").is_err(),
        "2100 is not a leap year (100 rule)"
    );
    assert!(
        load("2026-04-31", "2026-05-01").is_err(),
        "April has 30 days"
    );
    assert!(
        load("2026-13-01", "2026-13-02").is_err(),
        "there is no month 13"
    );
    assert!(
        load("2026-09-02", "2026-09-01").is_err(),
        "review cannot come due before the review"
    );
}

// ---------------------------------------------------------------------------
// Proxy boundary
// ---------------------------------------------------------------------------

/// An absolute-form request URI (`GET http://evil.example/api/chat`) must not steer the proxy off
/// its configured upstream — only the path and query may pass through.
#[test]
fn absolute_form_request_uris_cannot_redirect_the_proxy() {
    let target =
        proxy_target("http://127.0.0.1:11434", "http://evil.example/api/chat?x=1").unwrap();
    assert_eq!(target.host_str(), Some("127.0.0.1"));
    assert_eq!(target.path(), "/api/chat");
    assert_eq!(target.query(), Some("x=1"));
}

#[test]
fn recursive_upstream_detection_covers_localhost_aliases() {
    let recursive = ProxyConfig::new("127.0.0.1:11435", "http://localhost:11435", false);
    assert!(recursive.validate().is_err());

    let different_port = ProxyConfig::new("127.0.0.1:11435", "http://localhost:11434", false);
    assert!(different_port.validate().is_ok());
}

/// Regression: bracketed IPv6 loopback must not evade the recursive-upstream guard.
#[test]
fn recursive_upstream_detection_covers_ipv6_loopback() {
    let recursive = ProxyConfig::new("[::1]:11435", "http://[::1]:11435", false);
    assert!(
        recursive.validate().is_err(),
        "an IPv6 proxy pointing at its own listener is exactly as recursive as the IPv4 form"
    );
}

#[test]
fn cpu_backend_configuration_fails_closed_on_ambiguous_or_empty_assignments() {
    let same_endpoint = PlatformConfig::new(
        "127.0.0.1:11435",
        "http://localhost:11434",
        None,
        None,
        "intent-model",
    )
    .with_cpu_backend("http://127.0.0.1:11434/", ["embed-model"]);
    assert!(same_endpoint.validate().is_err());

    let no_models = PlatformConfig::new(
        "127.0.0.1:11435",
        "http://127.0.0.1:11434",
        None,
        None,
        "intent-model",
    )
    .with_cpu_backend("http://127.0.0.1:11436", Vec::<String>::new());
    assert!(no_models.validate().is_err());
}

// ---------------------------------------------------------------------------
// Runtime metrics
// ---------------------------------------------------------------------------

/// Ollama can report a zero duration for a trivially cached prompt; the rate must become null,
/// never a division by zero or infinity.
#[test]
fn runtime_metrics_reports_null_rates_for_zero_or_missing_durations() {
    let zero_duration = runtime_metrics(&json!({"eval_count": 5, "eval_duration": 0}));
    assert!(zero_duration["output_tokens_per_second"].is_null());

    let missing_fields = runtime_metrics(&json!({}));
    assert!(missing_fields["prompt_tokens_per_second"].is_null());
    assert!(missing_fields["output_tokens_per_second"].is_null());
}

// ---------------------------------------------------------------------------
// Policy generation from quality evidence
// ---------------------------------------------------------------------------

fn aggregate_file(models_json: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(
        file,
        r#"{{"suite":{{"id":"s","benchmark_date":"2026-08-27","review":{{"fresh":true,"review_due_at":"2026-09-27"}}}},"models":[{models_json}]}}"#
    )
    .unwrap();
    file
}

#[test]
fn qualification_ranks_best_first_regardless_of_file_order() {
    let file = aggregate_file(
        r#"{"id":"worse-1b","pass_at_1":0.81,"trial_budget":3},
           {"id":"better-1b","pass_at_1":0.95,"trial_budget":3}"#,
    );
    let installed = vec!["worse:1b".to_owned(), "better:1b".to_owned()];

    let (qualified, _) = qualify_from_aggregate(file.path(), &installed, 0.8, false).unwrap();

    assert_eq!(qualified.len(), 2);
    assert_eq!(qualified[0].model, "better:1b");
    assert_eq!(qualified[1].model, "worse:1b");
}

#[test]
fn a_malformed_aggregate_reports_a_parse_error_not_a_panic() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(b"{ not json at all").unwrap();
    let error = qualify_from_aggregate(file.path(), &["m:1b".to_owned()], 0.8, false).unwrap_err();
    assert!(error.to_string().contains("parse aggregate"));
}

/// Regression: a short tag whose slug prefixes a longer tag must not steal the longer model's
/// score.
#[test]
fn qualification_credits_the_model_that_was_actually_evaluated_on_slug_prefix_collisions() {
    let file =
        aggregate_file(r#"{"id":"qwen3-8b-instruct-bash","pass_at_1":0.9,"trial_budget":3}"#);
    // The shorter tag listed first — the order an estate can trivially produce.
    let installed = vec!["qwen3:8b".to_owned(), "qwen3:8b-instruct".to_owned()];

    let (qualified, _) = qualify_from_aggregate(file.path(), &installed, 0.8, false).unwrap();

    assert_eq!(
        qualified[0].model, "qwen3:8b-instruct",
        "the run was of qwen3:8b-instruct; crediting qwen3:8b writes a quality contract for a model with no evidence"
    );
}

/// Regression: duplicate evidence for a model is removed even when another model's score sorts
/// between the duplicate entries.
#[test]
fn a_model_qualified_by_two_runs_appears_once_even_when_scores_are_not_adjacent() {
    let file = aggregate_file(
        r#"{"id":"alpha-1b-agent-x","pass_at_1":0.95,"trial_budget":3},
           {"id":"beta-1b","pass_at_1":0.90,"trial_budget":3},
           {"id":"alpha-1b-agent-y","pass_at_1":0.85,"trial_budget":3}"#,
    );
    let installed = vec!["alpha:1b".to_owned(), "beta:1b".to_owned()];

    let (qualified, _) = qualify_from_aggregate(file.path(), &installed, 0.8, false).unwrap();

    let alpha_entries = qualified
        .iter()
        .filter(|entry| entry.model == "alpha:1b")
        .count();
    assert_eq!(
        alpha_entries, 1,
        "each model must appear once in the ranked list, keeping its best score"
    );
    assert_eq!(qualified.len(), 2);
}

// ---------------------------------------------------------------------------
// Natural-language intent guards
// ---------------------------------------------------------------------------

/// Regression: "photo" inside "photosynthesis" must not force a vision task or requirement.
#[test]
fn incidental_substrings_do_not_create_a_vision_requirement() {
    let calm = RouteIntent {
        task: TaskKind::Completion,
        objective: Objective::Balanced,
        context_tokens: None,
        requires_tools: false,
        requires_vision: false,
    };

    let (intent, _) = normalize_route_intent(calm, "Explain photosynthesis to a child.");

    assert!(
        !intent.requires_vision,
        "'photosynthesis' contains 'photo' but is not an image request"
    );
}
