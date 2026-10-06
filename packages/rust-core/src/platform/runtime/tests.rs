use super::super::admission::AdmissionPool;
use super::*;

fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn history_and_warming_environment_override_file_with_finite_validation() {
    let file = RuntimeFile {
        scopes: Some(super::super::ScopePolicy {
            max_count: 10,
            ..Default::default()
        }),
        warming: Some(super::super::WarmingPolicy {
            base_seconds: 120,
            ..Default::default()
        }),
        ..Default::default()
    };
    let env = |name: &str| match name {
        "FREELLAMA_SCOPE_MAX_COUNT" => Some("3".to_owned()),
        "FREELLAMA_KEEP_ALIVE_BASE_SECONDS" => Some("90".to_owned()),
        "FREELLAMA_USAGE_QUEUE_CAPACITY" => Some("8".to_owned()),
        _ => None,
    };
    let resolved = resolve(&PinnedTunables::default(), &file, 1, &env);
    assert_eq!(resolved.scopes.max_count, 3);
    assert_eq!(resolved.warming.base_seconds, 90);
    assert_eq!(resolved.usage_queue_capacity.get(), 8);
    assert_eq!(resolved.sources["scopes"], "env");
    assert_eq!(resolved.sources["warming"], "env");
    assert!(resolved.warming.validate().is_ok());
}

#[test]
fn reload_changes_scope_policy_but_rejects_startup_queue_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.toml");
    std::fs::write(
        &path,
        "usage_queue_capacity = 128\n[scopes]\nmax_count = 10\n",
    )
    .unwrap();
    let settings = RuntimeSettings::new(PinnedTunables::default(), Some(path.clone()), 1).unwrap();
    std::fs::write(
        &path,
        "usage_queue_capacity = 128\n[scopes]\nmax_count = 2\n",
    )
    .unwrap();
    assert!(settings.reload(true).unwrap().is_some());
    assert_eq!(settings.get().scopes.max_count, 2);
    std::fs::write(
        &path,
        "usage_queue_capacity = 129\n[scopes]\nmax_count = 5\n",
    )
    .unwrap();
    assert!(
        settings
            .reload(true)
            .unwrap_err()
            .contains("fixed at startup")
    );
    assert_eq!(settings.get().scopes.max_count, 2);
    std::fs::write(&path, "usage_queue_capacity = 0\n").unwrap();
    assert!(RuntimeFile::load(&path).is_err());
    std::fs::write(&path, "[warming]\nmin_seconds = 50\nbase_seconds = 10\n").unwrap();
    assert!(RuntimeFile::load(&path).is_err());
}

#[test]
fn precedence_is_cli_then_env_then_file_then_default() {
    let file = RuntimeFile {
        max_concurrent_tasks: Some(6),
        cpu_max_concurrent_tasks: Some(3),
        max_queued_tasks: Some(40),
        max_queue_wait_seconds: Some(30),
        ..RuntimeFile::default()
    };
    let pinned = PinnedTunables {
        max_concurrent_tasks: Some(8),
        ..PinnedTunables::default()
    };
    let env = env_from(&[("FREELLAMA_MAX_QUEUE_WAIT_SECONDS", "7")]);
    let tunables = resolve(&pinned, &file, 1, &env);
    assert_eq!(tunables.max_concurrent_tasks, 8);
    assert_eq!(tunables.sources["max_concurrent_tasks"], "cli");
    assert_eq!(tunables.max_queue_wait_ms, 7_000);
    assert_eq!(tunables.sources["max_queue_wait_ms"], "env");
    assert_eq!(tunables.cpu_max_concurrent_tasks, 3);
    assert_eq!(tunables.sources["cpu_max_concurrent_tasks"], "file");
    assert_eq!(tunables.max_queued_tasks, 40);
    assert_eq!(tunables.sources["max_queued_tasks"], "file");
    assert_eq!(tunables.cpu_max_queued_tasks, 8);
    assert_eq!(tunables.sources["cpu_max_queued_tasks"], "default");
}

#[test]
fn gpu_default_follows_ollama_num_parallel() {
    let none = env_from(&[]);
    let tunables = resolve(
        &PinnedTunables::default(),
        &RuntimeFile::default(),
        3,
        &none,
    );
    assert_eq!(tunables.max_concurrent_tasks, 6);
    assert_eq!(
        tunables.sources["max_concurrent_tasks"],
        "ollama_num_parallel"
    );
    let single = resolve(
        &PinnedTunables::default(),
        &RuntimeFile::default(),
        1,
        &none,
    );
    assert_eq!(single.max_concurrent_tasks, 2);
}

#[test]
fn enums_and_lists_parse_from_file() {
    let file: RuntimeFile = toml::from_str(
        "context_mode = \"explicit\"\nadaptive_concurrency = \"all\"\nbreaker_failures = 0\npinned_models = [\"a:1\", \"b:2\"]\n",
    )
    .unwrap();
    let tunables = resolve(&PinnedTunables::default(), &file, 1, &env_from(&[]));
    assert_eq!(tunables.adaptive_concurrency, AdaptiveMode::All);
    assert_eq!(tunables.context_mode, ContextMode::Explicit);
    assert_eq!(tunables.breaker_failures, 0);
    assert_eq!(
        tunables.pinned_models,
        BTreeSet::from(["a:1".to_owned(), "b:2".to_owned()])
    );
}

#[test]
fn unknown_file_fields_are_rejected() {
    assert!(toml::from_str::<RuntimeFile>("max_concurent_tasks = 2").is_err());
}

#[test]
fn reload_picks_up_file_edits_keeps_pins_and_survives_bad_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.toml");
    std::fs::write(&path, "max_queued_tasks = 5\n").unwrap();
    let settings = RuntimeSettings::new(
        PinnedTunables {
            cpu_max_queued_tasks: Some(2),
            ..PinnedTunables::default()
        },
        Some(path.clone()),
        1,
    )
    .unwrap();
    assert_eq!(settings.get().max_queued_tasks, 5);
    std::fs::write(&path, "max_queued_tasks = 9\ncpu_max_queued_tasks = 50\n").unwrap();
    let changed = settings.reload(true).unwrap().expect("values changed");
    assert_eq!(changed.max_queued_tasks, 9);
    assert_eq!(changed.cpu_max_queued_tasks, 2, "a CLI pin beats the file");
    std::fs::write(&path, "max_queued_tasks = \"nine\"\n").unwrap();
    assert!(settings.reload(true).is_err());
    assert_eq!(
        settings.get().max_queued_tasks,
        9,
        "a bad file keeps the last good values"
    );
    assert!(settings.receipt()["reload"]["last_error"].is_string());
}

#[test]
fn breaker_opens_after_threshold_and_half_opens_after_cooldown() {
    let breakers = Breakers::default();
    let cooldown = Duration::from_millis(30);
    assert!(breakers.check("gpu").is_ok());
    assert!(!breakers.record("gpu", false, 2, cooldown));
    assert!(breakers.record("gpu", false, 2, cooldown));
    assert!(breakers.check("gpu").is_err());
    std::thread::sleep(Duration::from_millis(40));
    let probe = breakers.check("gpu").expect("one probe after the cooldown");
    assert!(probe.is_some());
    assert!(breakers.check("gpu").is_err(), "only one probe at a time");
    drop(probe);
    let probe = breakers
        .check("gpu")
        .expect("an abandoned probe frees the slot");
    assert!(
        breakers.record("gpu", false, 2, cooldown),
        "a failed probe re-opens"
    );
    drop(probe);
    assert!(breakers.check("gpu").is_err());
    std::thread::sleep(Duration::from_millis(40));
    let probe = breakers.check("gpu").unwrap();
    assert!(!breakers.record("gpu", true, 2, cooldown));
    drop(probe);
    assert!(breakers.check("gpu").is_ok());
    assert_eq!(breakers.receipt()["gpu"]["state"], "closed");
}

#[test]
fn breaker_threshold_zero_disables_it() {
    let breakers = Breakers::default();
    for _ in 0..10 {
        assert!(!breakers.record("cpu", false, 0, Duration::from_secs(1)));
    }
    assert!(breakers.check("cpu").is_ok());
}

#[test]
fn adaptive_limit_halves_on_trouble_and_climbs_back_slowly() {
    let pool = AdmissionPool::new(8, 4);
    let limiter = AdaptiveLimiter::default();
    let profile = test_profile("profile-a");
    let healthy = |tps| Completion {
        profile: Some(&profile),
        failed: false,
        memory_pressure: false,
        metric: Some(ThroughputMetric {
            kind: ThroughputMetricKind::OutputTokensPerSecond,
            tokens_per_second: tps,
        }),
    };
    for _ in 0..3 {
        assert_eq!(limiter.observe(&pool, &healthy(40.0)), None);
    }
    assert_eq!(
        limiter.observe(&pool, &healthy(10.0)),
        Some(("decrease", 4))
    );
    // A second bad sample inside the spacing window does not compound the cut.
    assert_eq!(limiter.observe(&pool, &healthy(10.0)), None);
    assert_eq!(pool.total(), 4);
    for _ in 0..4 {
        assert_eq!(limiter.observe(&pool, &healthy(40.0)), None);
    }
    assert_eq!(
        limiter.observe(&pool, &healthy(40.0)),
        Some(("increase", 5))
    );
    assert_eq!(pool.ceiling(), 8);
}

#[test]
fn pressure_and_failures_count_as_bad_even_without_throughput() {
    let pool = AdmissionPool::new(4, 4);
    let limiter = AdaptiveLimiter::default();
    let change = limiter.observe(
        &pool,
        &Completion {
            profile: None,
            failed: false,
            memory_pressure: true,
            metric: None,
        },
    );
    assert_eq!(change, Some(("decrease", 2)));
    assert_eq!(
        limiter.receipt(&pool, true)["last_decrease_reason"],
        "host_memory_pressure"
    );
}

fn test_profile(id: &str) -> ExecutionProfile {
    ExecutionProfile {
        id: id.into(),
        task: super::super::TaskKind::Completion,
        model: "m".into(),
        digest: "revision-a".into(),
        context_tokens: 4096,
        processor: "gpu".into(),
        metric_kind: ThroughputMetricKind::OutputTokensPerSecond,
    }
}

#[test]
fn unknown_or_nonserial_intervals_break_consecutive_recovery() {
    let pool = AdmissionPool::new(8, 4);
    let limiter = AdaptiveLimiter::default();
    let profile = test_profile("serial");
    let healthy = Completion {
        profile: Some(&profile),
        failed: false,
        memory_pressure: false,
        metric: Some(ThroughputMetric {
            kind: ThroughputMetricKind::OutputTokensPerSecond,
            tokens_per_second: 40.0,
        }),
    };
    for _ in 0..3 {
        limiter.observe(&pool, &healthy);
    }
    let unknown_parallel_interval = Completion {
        profile: None,
        metric: Some(ThroughputMetric {
            kind: ThroughputMetricKind::OutputTokensPerSecond,
            tokens_per_second: 20.0,
        }),
        ..healthy
    };
    limiter.observe(&pool, &unknown_parallel_interval);
    assert_eq!(
        limiter.receipt(&pool, true)["healthy_streak"],
        0,
        "unknown parallel interval cannot bridge a consecutive serial streak"
    );
    for tps in [
        None,
        Some(0.0),
        Some(-1.0),
        Some(f64::NAN),
        Some(f64::INFINITY),
    ] {
        limiter.observe(&pool, &healthy);
        limiter.observe(
            &pool,
            &Completion {
                metric: tps.map(|tokens_per_second| ThroughputMetric {
                    kind: ThroughputMetricKind::OutputTokensPerSecond,
                    tokens_per_second,
                }),
                ..healthy
            },
        );
        assert_eq!(
            limiter.receipt(&pool, true)["healthy_streak"],
            0,
            "missing or invalid rate must reset recovery"
        );
    }
}

#[test]
fn incomplete_or_cancelled_completions_reset_the_recovery_streak() {
    let pool = AdmissionPool::new(8, 4);
    let limiter = AdaptiveLimiter::default();
    let profile = test_profile("serial");
    let healthy = Completion {
        profile: Some(&profile),
        failed: false,
        memory_pressure: false,
        metric: Some(ThroughputMetric {
            kind: ThroughputMetricKind::OutputTokensPerSecond,
            tokens_per_second: 40.0,
        }),
    };
    for _ in 0..3 {
        limiter.observe(&pool, &healthy);
    }
    let incomplete = limiter.completion_guard();
    drop(incomplete);
    limiter.observe(&pool, &healthy);
    assert_eq!(limiter.receipt(&pool, true)["healthy_streak"], 1);
    let mut complete = limiter.completion_guard();
    limiter.observe(&pool, &healthy);
    complete.complete();
    drop(complete);
    assert_eq!(limiter.receipt(&pool, true)["healthy_streak"], 2);
}

#[test]
fn profile_windows_are_bounded_by_task_kinds_and_fully_visible() {
    let pool = AdmissionPool::new(8, 4);
    let limiter = AdaptiveLimiter::default();
    let tasks = <super::super::TaskKind as clap::ValueEnum>::value_variants();
    for task in tasks {
        for revision in 0..20 {
            let mut profile = test_profile(&format!("{task:?}-{revision}"));
            profile.task = *task;
            limiter.observe(
                &pool,
                &Completion {
                    profile: Some(&profile),
                    failed: false,
                    memory_pressure: false,
                    metric: Some(ThroughputMetric {
                        kind: ThroughputMetricKind::OutputTokensPerSecond,
                        tokens_per_second: 40.0,
                    }),
                },
            );
        }
    }
    let receipt = limiter.receipt(&pool, true);
    assert_eq!(receipt["current_profile_count"], tasks.len());
    assert_eq!(
        receipt["profile_windows"].as_object().unwrap().len(),
        tasks.len(),
        "same model across tasks must not collapse diagnostic windows"
    );
}

#[test]
fn retry_after_scales_with_queue_and_is_bounded() {
    assert_eq!(retry_after_seconds(0, 2, Some(4_000)), 2);
    assert_eq!(retry_after_seconds(9, 2, Some(4_000)), 20);
    assert_eq!(retry_after_seconds(10_000, 1, Some(60_000)), 120);
    assert_eq!(retry_after_seconds(0, 4, None), 2);
}
