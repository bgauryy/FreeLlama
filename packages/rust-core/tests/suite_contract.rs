use freellama::{
    Comparison, GuardrailStatus, RunReport, Suite, Verdict, compare,
    local_conservative_config_posture, max_loaded_models_advisory, ollama_config_diagnostics,
    parse_macos_thermal_status, parse_ollama_cli_version, parse_ollama_process_environment,
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
fn local_conservative_posture_prefers_observed_service_settings_over_hints() {
    let config = ollama_config_diagnostics("0.33.2", |_| None);
    let observed = serde_json::json!({
        "status": "observed",
        "settings": {
            "OLLAMA_NO_CLOUD": "1",
            "OLLAMA_MAX_LOADED_MODELS": "1",
            "OLLAMA_NUM_PARALLEL": "1",
            "OLLAMA_MAX_QUEUE": "16"
        }
    });
    let posture = local_conservative_config_posture(&config, &observed);
    assert_eq!(posture["profile"], "local-conservative-v1");
    assert_eq!(posture["overall"], "ready");
    assert_eq!(
        posture["settings"]["OLLAMA_NO_CLOUD"]["source"],
        "observed_process"
    );
    assert_eq!(posture["settings"]["OLLAMA_MAX_QUEUE"]["status"], "ready");
    assert!(
        posture["apply"].is_object(),
        "must provide non-mutating operator guidance"
    );
}

#[test]
fn local_conservative_posture_warns_when_cloud_and_unbounded_queue_are_visible() {
    let config = ollama_config_diagnostics("0.33.2", |_| None);
    let observed = serde_json::json!({
        "status": "observed",
        "settings": { "OLLAMA_NO_CLOUD": "0" }
    });
    let posture = local_conservative_config_posture(&config, &observed);
    assert_eq!(posture["overall"], "action_required");
    assert_eq!(
        posture["settings"]["OLLAMA_NO_CLOUD"]["status"],
        "action_required"
    );
    assert_eq!(
        posture["settings"]["OLLAMA_MAX_QUEUE"]["status"],
        "recommended"
    );
}

#[test]
fn mac_thermal_parser_returns_a_small_status_not_raw_system_output() {
    assert_eq!(
        parse_macos_thermal_status("Note: No thermal warning level has been recorded\n"),
        serde_json::json!({ "status": "normal" })
    );
    assert_eq!(
        parse_macos_thermal_status("Note: Thermal warning level: 2\n"),
        serde_json::json!({ "status": "warning", "level": "2" })
    );
}

#[test]
fn process_environment_parser_is_allow_listed_and_never_leaks_unrelated_values() {
    let values = parse_ollama_process_environment(
        "/Applications/Ollama.app/Contents/Resources/ollama serve API_TOKEN=never OLLAMA_NO_CLOUD=1 OLLAMA_NUM_PARALLEL=2 SECRET=never LLAMA_ARG_FIT=0",
    );
    assert_eq!(values.get("OLLAMA_NO_CLOUD").map(String::as_str), Some("1"));
    assert_eq!(
        values.get("OLLAMA_NUM_PARALLEL").map(String::as_str),
        Some("2")
    );
    assert_eq!(values.get("LLAMA_ARG_FIT").map(String::as_str), Some("0"));
    assert!(!values.contains_key("API_TOKEN"));
    assert!(!values.contains_key("SECRET"));
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

/// Every advisory must state what Ollama resolves when the variable is unset. A bare `null` value
/// with no `effective_default` reads as "off", which is exactly the misreading that produced two
/// wrong advisories in this table.
#[test]
fn every_ollama_env_advisory_states_its_effective_default() {
    let table = freellama::ollama_env_advisories(|_| None);
    let entries = table.as_object().expect("advisory table is an object");
    assert_eq!(
        entries.len(),
        11,
        "doctor documents eleven memory-governing settings: nine OLLAMA_* plus LLAMA_ARG_FIT and \
         LLAMA_ARG_FIT_TARGET, which govern memory but lack the prefix an auditor greps for"
    );
    for (key, entry) in entries {
        assert!(
            entry
                .get("effective_default")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "{key} has no effective_default — an unset value would read as \"off\""
        );
    }
}

/// Regression: flash attention is NOT off by default. envconfig declares it with
/// `BoolWithDefault` (the caller supplies the default; plain `Bool` is the one pinned to false)
/// and docs/faq.mdx states Ollama "uses Flash Attention automatically when the selected backend
/// and devices support it". Reporting "off" told users to set a variable that is already on, and
/// implied the `q8_0` KV-cache saving was unavailable to them — a real memory lever, wrongly closed.
#[test]
fn flash_attention_is_not_advertised_as_off_by_default() {
    let table = freellama::ollama_env_advisories(|_| None);
    let default = table["OLLAMA_FLASH_ATTENTION"]["effective_default"]
        .as_str()
        .expect("flash attention advisory");
    assert!(
        !default.eq_ignore_ascii_case("off"),
        "flash attention is auto-enabled on supported backends, not off; got {default:?}"
    );
    assert!(
        default.contains("auto"),
        "the advisory should say the default is automatic; got {default:?}"
    );
}

/// The injected `getenv` must actually reach the table, or the reported `value` would silently be
/// whatever the host machine has set rather than what the caller asked about.
#[test]
fn ollama_env_advisories_report_the_configured_value() {
    let table = freellama::ollama_env_advisories(|key| {
        (key == "OLLAMA_MAX_LOADED_MODELS").then(|| "1".to_owned())
    });
    assert_eq!(table["OLLAMA_MAX_LOADED_MODELS"]["value"], "1");
    assert_eq!(
        table["OLLAMA_NUM_PARALLEL"]["value"],
        serde_json::Value::Null
    );
}

/// `ollama_env_config` remains the stable flat memory view, while the categorized view covers the
/// rest of the production settings exposed by the detected Ollama generation.
#[test]
fn categorized_ollama_config_covers_installed_production_settings() {
    let report = ollama_config_diagnostics("0.33.2", |_| None);
    assert_eq!(report["server_version"], "0.33.2");
    assert_eq!(report["coverage"]["known_settings"], 21);

    let categories = report["categories"].as_object().expect("categories object");
    for category in [
        "memory_scheduler",
        "network_security",
        "privacy",
        "storage_lifecycle",
        "backend_device",
        "operations",
    ] {
        assert!(categories.contains_key(category), "missing {category}");
    }

    let expected = [
        ("network_security", "OLLAMA_HOST"),
        ("network_security", "OLLAMA_ORIGINS"),
        ("privacy", "OLLAMA_NO_CLOUD"),
        ("storage_lifecycle", "OLLAMA_MODELS"),
        ("storage_lifecycle", "OLLAMA_NOPRUNE"),
        ("backend_device", "OLLAMA_LLM_LIBRARY"),
        ("backend_device", "OLLAMA_SCHED_SPREAD"),
        ("backend_device", "OLLAMA_IGPU_ENABLE"),
        ("operations", "OLLAMA_MAX_TRANSFER_STREAMS"),
        ("operations", "OLLAMA_DEBUG"),
    ];
    for (category, setting) in expected {
        let entry = &report["categories"][category][setting];
        assert!(entry.is_object(), "missing {category}.{setting}");
        assert!(
            entry["effective_default"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "{category}.{setting} needs an honest effective default"
        );
    }
}

#[test]
fn categorized_ollama_config_reports_values_without_claiming_process_visibility() {
    let report = ollama_config_diagnostics("0.33.2", |key| Some(format!("configured:{key}")));
    for (category, setting) in [
        ("network_security", "OLLAMA_HOST"),
        ("network_security", "OLLAMA_ORIGINS"),
        ("privacy", "OLLAMA_NO_CLOUD"),
        ("storage_lifecycle", "OLLAMA_MODELS"),
        ("storage_lifecycle", "OLLAMA_NOPRUNE"),
        ("backend_device", "OLLAMA_LLM_LIBRARY"),
        ("backend_device", "OLLAMA_SCHED_SPREAD"),
        ("backend_device", "OLLAMA_IGPU_ENABLE"),
        ("operations", "OLLAMA_MAX_TRANSFER_STREAMS"),
        ("operations", "OLLAMA_DEBUG"),
    ] {
        assert_eq!(
            report["categories"][category][setting]["value"],
            format!("configured:{setting}"),
            "configured value missing for {category}.{setting}"
        );
    }
    assert!(
        report["visibility_note"]
            .as_str()
            .expect("visibility note")
            .contains("cannot prove")
    );
}

/// `NUM_PARALLEL=1` limits concurrency within one Ollama process, but separate CPU and GPU
/// processes can overlap. The advisory must not collapse those two resource scopes.
#[test]
fn num_parallel_advisory_explains_the_admission_interaction() {
    let table = freellama::ollama_env_advisories(|_| None);
    let note = table["OLLAMA_NUM_PARALLEL"]["note"].as_str().expect("note");
    assert!(
        note.contains("SHARED"),
        "must name the shared admission permit; got {note:?}"
    );
    assert!(
        note.contains("serializes"),
        "must say Ollama serializes them at the default; got {note:?}"
    );
    assert!(
        note.contains("Separate CPU and GPU Ollama processes can still overlap"),
        "must preserve the proven cross-backend concurrency contract; got {note:?}"
    );
}

#[test]
fn q8_kv_cache_advisory_names_both_memory_gain_and_quality_gate() {
    let table = freellama::ollama_env_advisories(|_| None);
    let note = table["OLLAMA_KV_CACHE_TYPE"]["note"]
        .as_str()
        .expect("note");
    assert!(
        note.contains("roughly halves"),
        "must quantify the memory gain"
    );
    assert!(
        note.contains("very small"),
        "must report upstream's precision characterization"
    );
    assert!(
        note.contains("qualify model quality"),
        "must gate the process-wide tradeoff"
    );
}
