//! Adversarial edge cases the per-module contract tests do not pin down: boundary arithmetic,
//! tie-breaking stability, hostile/malformed inputs, fail-closed defaults, and fixed regressions.

use std::collections::BTreeMap;

use freellama::model_bench::{Capability, ModelMetadata, ModelType, benchmark_plan};
use freellama::parse_ollama_cli_version;
use freellama::platform::{
    CatalogModel, Objective, RouteInput, SessionAffinity, TaskKind, runtime_metrics, select_route,
};
use freellama::proxy::{ProxyConfig, proxy_target};
use serde_json::json;

fn model(name: &str, size: u64, capabilities: &[Capability]) -> CatalogModel {
    CatalogModel {
        name: name.to_owned(),
        digest: None,
        size,
        capabilities: capabilities.iter().copied().collect(),
        other_capabilities: Vec::new(),
        advertised_context: Some(32_768),
        kv_cache_bytes_per_token_f16: None,
        modelfile_num_ctx: None,
        architecture: None,
        resident: false,
        resident_size: None,
        resident_vram: None,
        resident_context: None,
        benchmark: BTreeMap::new(),
        observed_loops: BTreeMap::new(),
        policy_rank: BTreeMap::new(),
        policy_options: BTreeMap::new(),
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

/// A wildcard listener answers on every loopback address, so any loopback upstream on the same
/// port is the proxy itself.
#[test]
fn recursive_upstream_detection_covers_wildcard_listeners() {
    for (listen, upstream) in [
        ("0.0.0.0:11435", "http://127.0.0.1:11435"),
        ("0.0.0.0:11435", "http://localhost:11435"),
        ("0.0.0.0:11435", "http://127.8.9.10:11435"),
        ("[::]:11435", "http://[::1]:11435"),
        ("[::]:11435", "http://localhost:11435"),
        ("127.0.0.1:11435", "http://127.8.9.10:11435"),
    ] {
        let error = ProxyConfig::new(listen, upstream, true)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("points back"),
            "{listen} -> {upstream}: {error}"
        );
    }
    assert!(
        ProxyConfig::new("0.0.0.0:11435", "http://127.0.0.1:11434", true)
            .validate()
            .is_ok()
    );
}

#[test]
fn unspecified_upstream_hosts_are_rejected_with_a_concrete_hint() {
    for upstream in ["http://0.0.0.0:11434", "http://[::]:11434"] {
        let error = ProxyConfig::new("127.0.0.1:11435", upstream, false)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("use 127.0.0.1"), "{upstream}: {error}");
        let error = crate::support::platform_config("127.0.0.1:11435", upstream, None, None)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("use 127.0.0.1"), "{upstream}: {error}");
        let error = crate::support::platform_config(
            "127.0.0.1:11435",
            "http://127.0.0.1:11434",
            None,
            None,
        )
        .with_cpu_backend(upstream, ["embed-model"])
        .validate()
        .unwrap_err()
        .to_string();
        assert!(error.contains("use 127.0.0.1"), "cpu {upstream}: {error}");
    }
}

#[test]
fn cpu_backend_configuration_fails_closed_on_ambiguous_or_empty_assignments() {
    let same_endpoint =
        crate::support::platform_config("127.0.0.1:11435", "http://localhost:11434", None, None)
            .with_cpu_backend("http://127.0.0.1:11434/", ["embed-model"]);
    assert!(same_endpoint.validate().is_err());

    let no_models =
        crate::support::platform_config("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
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

/// A caller's model name resolves the way Ollama reads it: the exact tag, else `name:latest`,
/// else the one installed tag of that family. Unknown and ambiguous names are refused with the
/// installed candidates, never substituted.
#[test]
fn model_names_resolve_to_installed_tags_like_ollama() {
    use freellama::platform::installed_model;
    let models = vec![
        model("glm-ocr:latest", 2_900_000_000, &[Capability::Vision]),
        model("qwen3:8b", 5_000_000_000, &[Capability::Completion]),
        model("qwen3:1.7b", 1_400_000_000, &[Capability::Completion]),
    ];
    assert_eq!(
        installed_model(&models, "glm-ocr:latest").unwrap().name,
        "glm-ocr:latest"
    );
    assert_eq!(
        installed_model(&models, "glm-ocr").unwrap().name,
        "glm-ocr:latest"
    );
    assert_eq!(
        installed_model(&models, "qwen3:8b").unwrap().name,
        "qwen3:8b"
    );
    let ambiguous = installed_model(&models, "qwen3").unwrap_err().to_string();
    assert!(
        ambiguous.contains("ambiguous")
            && ambiguous.contains("qwen3:8b")
            && ambiguous.contains("qwen3:1.7b"),
        "{ambiguous}"
    );
    let missing = installed_model(&models, "nope").unwrap_err().to_string();
    assert!(
        missing.contains("not installed: nope") && missing.contains("glm-ocr:latest"),
        "{missing}"
    );
    let only = vec![model("nomic-embed-text:v1.5", 1, &[Capability::Embedding])];
    assert_eq!(
        installed_model(&only, "nomic-embed-text").unwrap().name,
        "nomic-embed-text:v1.5"
    );

    // select_route accepts the bare name and reports the resolved tag.
    let route = select_route(
        &RouteInput {
            task: TaskKind::Vision,
            model: Some("glm-ocr".into()),
            required_capabilities: [Capability::Vision].into_iter().collect(),
            ..RouteInput::default()
        },
        &models,
        &SessionAffinity::default(),
    )
    .unwrap();
    assert_eq!(route.selected_model, "glm-ocr:latest");
}
