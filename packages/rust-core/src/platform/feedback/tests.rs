use super::super::{Backend, ExecutionPreference, Objective, TaskKind, feedback_work_unit_ns};
use super::{FeedbackStats, PlacementSignals, desired_placement, meaningfully_faster};
use serde_json::json;

#[test]
fn two_processes_saving_feedback_keep_the_longer_history_per_task() {
    let store = crate::store::Store::in_memory().unwrap();
    let stats = |completed| FeedbackStats {
        completed,
        ..FeedbackStats::default()
    };
    let mut first = super::PlacementFeedback::default();
    first.gpu.insert(TaskKind::Completion, stats(9));
    super::persist_feedback(&store, &first).unwrap();
    // A second process that started earlier knows less about completion, more about embedding.
    let mut second = super::PlacementFeedback::default();
    second.gpu.insert(TaskKind::Completion, stats(2));
    second.gpu.insert(TaskKind::Embedding, stats(4));
    super::persist_feedback(&store, &second).unwrap();
    let saved = super::load_feedback(&store).unwrap();
    assert_eq!(saved.gpu[&TaskKind::Completion].completed, 9);
    assert_eq!(saved.gpu[&TaskKind::Embedding].completed, 4);
}

#[test]
fn backend_feedback_requires_a_real_ten_percent_advantage() {
    assert!(!meaningfully_faster(90, 100));
    assert!(!meaningfully_faster(95, 100));
    assert!(meaningfully_faster(89, 100));
}

#[test]
fn placement_decision_covers_every_signal_permutation() {
    let objectives = [Objective::Fastest, Objective::Balanced, Objective::Quality];
    let preferences = [
        ExecutionPreference::Auto,
        ExecutionPreference::PreferCpu,
        ExecutionPreference::PreferGpu,
    ];
    let gpu_scores = [None, Some(100)];
    let cpu_scores = [None, Some(89), Some(90), Some(100), Some(111), Some(112)];
    let mut checked = 0;

    for route_is_pinned in [false, true] {
        for objective in objectives {
            for execution_preference in preferences {
                for gpu_work_unit_ns in gpu_scores {
                    for cpu_work_unit_ns in cpu_scores {
                        for gpu_slots_available in [0, 1, 4] {
                            for cpu_slots_available in [0, 1, 4] {
                                for gpu_task_cost in [1, 2, 4] {
                                    for cpu_task_cost in [1, 2, 4] {
                                        for cpu_configured in [false, true] {
                                            for (gpu_resident, cpu_resident) in RESIDENCY {
                                                let signals = PlacementSignals {
                                                    route_is_pinned,
                                                    objective,
                                                    execution_preference,
                                                    gpu_work_unit_ns,
                                                    cpu_work_unit_ns,
                                                    gpu_slots_available,
                                                    cpu_slots_available,
                                                    gpu_task_cost,
                                                    cpu_task_cost,
                                                    cpu_configured,
                                                    gpu_resident,
                                                    cpu_resident,
                                                };
                                                let expected = placement_oracle(signals);
                                                assert_eq!(
                                                    desired_placement(signals),
                                                    expected,
                                                    "placement mismatch for {signals:?}"
                                                );
                                                checked += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    assert_eq!(checked, 2 * 3 * 3 * 2 * 6 * 3 * 3 * 3 * 3 * 2 * 4);
}

const RESIDENCY: [(bool, bool); 4] = [(false, false), (false, true), (true, false), (true, true)];

fn placement_oracle(signals: PlacementSignals) -> Option<(Backend, &'static str)> {
    let gpu_ready = signals.gpu_slots_available >= signals.gpu_task_cost;
    let cpu_ready = signals.cpu_configured && signals.cpu_slots_available >= signals.cpu_task_cost;
    match signals.execution_preference {
        ExecutionPreference::PreferCpu => {
            return Some(if cpu_ready || !gpu_ready {
                (Backend::Cpu, "preferred_backend_eligible")
            } else {
                (Backend::Gpu, "backend_capacity_available")
            });
        }
        ExecutionPreference::PreferGpu => {
            return Some(if gpu_ready || !cpu_ready {
                (Backend::Gpu, "preferred_backend_eligible")
            } else {
                (Backend::Cpu, "backend_capacity_available")
            });
        }
        ExecutionPreference::Auto => {}
    }
    if signals.route_is_pinned || matches!(signals.objective, Objective::Quality) {
        return None;
    }
    match (signals.gpu_work_unit_ns, signals.cpu_work_unit_ns) {
        (Some(gpu), Some(cpu)) if meaningfully_faster(cpu, gpu) => {
            return Some(
                if !signals.cpu_resident && signals.gpu_resident && gpu_ready {
                    (Backend::Gpu, "resident_backend_avoids_cold_load")
                } else if cpu_ready || !gpu_ready {
                    (Backend::Cpu, "measured_backend_faster")
                } else {
                    (Backend::Gpu, "backend_capacity_available")
                },
            );
        }
        (Some(gpu), Some(cpu)) if meaningfully_faster(gpu, cpu) => {
            return Some(
                if !signals.gpu_resident && signals.cpu_resident && cpu_ready {
                    (Backend::Cpu, "resident_backend_avoids_cold_load")
                } else if gpu_ready || !cpu_ready {
                    (Backend::Gpu, "measured_backend_faster")
                } else {
                    (Backend::Cpu, "backend_capacity_available")
                },
            );
        }
        _ => {}
    }
    if !gpu_ready && cpu_ready {
        return Some((Backend::Cpu, "backend_capacity_available"));
    }
    if !cpu_ready && gpu_ready {
        return Some((Backend::Gpu, "backend_capacity_available"));
    }
    None
}

#[test]
fn feedback_readiness_covers_sample_and_duration_permutations() {
    let mut checked = 0;
    for duration_samples in 0..=4 {
        for total_work_unit_ns in [0, 100] {
            let stats = FeedbackStats {
                duration_samples,
                total_work_unit_ns,
                ..FeedbackStats::default()
            };
            let expected = (duration_samples >= 3 && total_work_unit_ns > 0)
                .then_some(total_work_unit_ns / u128::from(duration_samples.max(1)));
            assert_eq!(stats.average_work_unit_ns(), expected);
            checked += 1;
        }
    }
    assert_eq!(checked, 10);
}

#[test]
fn feedback_never_combines_different_models_in_one_backend_bucket() {
    let mut stats = FeedbackStats::default();
    for _ in 0..3 {
        stats.record("model-a", Some(100), 4);
    }
    assert_eq!(stats.average_work_unit_ns(), Some(100));
    assert_eq!(stats.profile, None);

    stats.record("model-b", Some(50), 2);
    assert_eq!(stats.model.as_deref(), Some("model-b"));
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.duration_samples, 1);
    assert_eq!(stats.profile, None);
}

#[test]
fn aggregate_diagnostics_never_become_profile_evidence() {
    let mut stats = FeedbackStats::default();
    for _ in 0..5 {
        stats.record("model-a", Some(100), 2);
    }
    assert_eq!(stats.average_work_unit_ns(), Some(100));
    assert_eq!(stats.receipt()["decision_ready"], false);
    let profile = super::runtime::ExecutionProfile {
        id: "profile-a".into(),
        task: TaskKind::Completion,
        model: "model-a".into(),
        digest: "revision-a".into(),
        context_tokens: 4096,
        processor: "gpu".into(),
        metric_kind: super::runtime::ThroughputMetricKind::OutputTokensPerSecond,
    };
    for _ in 0..3 {
        stats.record_profile(&profile, 50);
    }
    assert_eq!(stats.average_for_profile(&profile), Some(50));
    let stored = serde_json::to_value(&stats).unwrap();
    assert_eq!(stored["completed"], 5);
    assert_eq!(stored["total_work_unit_ns"], 500);
    let restarted: FeedbackStats = serde_json::from_value(stored).unwrap();
    assert_eq!(restarted.average_for_profile(&profile), Some(50));
    let mut changed = profile.clone();
    changed.id = "profile-b".into();
    stats.record_profile(&changed, 10);
    assert_eq!(stats.average_for_profile(&profile), Some(50));
    assert_eq!(stats.average_for_profile(&changed), None);
    assert_eq!(stats.profile_duration_samples, 1);
}

fn profile(name: &str) -> super::runtime::ExecutionProfile {
    super::runtime::ExecutionProfile {
        id: format!("profile-{name}"),
        task: TaskKind::Completion,
        model: name.into(),
        digest: format!("digest-{name}"),
        context_tokens: 4096,
        processor: "gpu".into(),
        metric_kind: super::runtime::ThroughputMetricKind::OutputTokensPerSecond,
    }
}

fn learn(stats: &mut FeedbackStats, profile: &super::runtime::ExecutionProfile, rate: u64) {
    stats.record(&profile.model, Some(rate), 0);
    stats.record_profile(profile, rate);
}

#[test]
fn alternating_models_keep_separate_exact_profile_evidence() {
    let (a, b) = (profile("a"), profile("b"));
    let mut stats = FeedbackStats::default();
    for _ in 0..3 {
        learn(&mut stats, &a, 100);
        learn(&mut stats, &b, 200);
    }
    assert_eq!(stats.average_for_profile(&a), Some(100));
    assert_eq!(stats.average_for_profile(&b), Some(200));
    // Even an accidentally reused opaque id cannot transfer evidence across a revision,
    // context, processor, model, task, or metric kind.
    for field in [
        "digest",
        "context",
        "processor",
        "model",
        "task",
        "metric",
        "id",
    ] {
        let mut changed = a.clone();
        match field {
            "digest" => changed.digest = "replacement".into(),
            "context" => changed.context_tokens = 8192,
            "processor" => changed.processor = "cpu".into(),
            "model" => changed.model = "other".into(),
            "task" => changed.task = TaskKind::Coding,
            "metric" => {
                changed.metric_kind = super::runtime::ThroughputMetricKind::InputTokensPerSecond;
            }
            _ => changed.id = "other-runtime-controls".into(),
        }
        assert_eq!(stats.average_for_profile(&changed), None, "{field}");
    }
}

#[test]
fn restarted_history_unions_process_samples_without_double_counting() {
    let store = crate::store::Store::in_memory().unwrap();
    let (a, b) = (profile("a"), profile("b"));
    let mut initial = super::PlacementFeedback::default();
    for _ in 0..12 {
        learn(
            initial.gpu.entry(TaskKind::Completion).or_default(),
            &a,
            100,
        );
    }
    super::persist_feedback(&store, &initial).unwrap();
    let mut first = super::load_feedback(&store).unwrap();
    let mut second = first.clone();
    learn(first.gpu.get_mut(&TaskKind::Completion).unwrap(), &a, 200);
    for _ in 0..3 {
        learn(second.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 50);
    }
    // A newer model has fewer diagnostic completions, but retains its evidence on restart.
    super::persist_feedback(&store, &second).unwrap();
    super::persist_feedback(&store, &first).unwrap();
    super::persist_feedback(&store, &first).unwrap();
    let saved = super::load_feedback(&store).unwrap();
    let stats = &saved.gpu[&TaskKind::Completion];
    assert_eq!(stats.average_for_profile(&a), Some(1400 / 13));
    assert_eq!(stats.average_for_profile(&b), Some(50));
    assert_eq!(stats.profile_window(&a).unwrap().samples.len(), 13);
    assert_eq!(stats.profile_window(&b).unwrap().samples.len(), 3);
    assert_eq!(stats.model.as_deref(), Some("b"));
    assert_eq!(stats.completed, 3);

    // Two processes branching from the same saved history contribute distinct observations.
    let mut left = saved.clone();
    let mut right = saved;
    learn(left.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 100);
    learn(right.gpu.get_mut(&TaskKind::Completion).unwrap(), &b, 200);
    super::persist_feedback(&store, &left).unwrap();
    super::persist_feedback(&store, &right).unwrap();
    let final_state = super::load_feedback(&store).unwrap();
    let stats = &final_state.gpu[&TaskKind::Completion];
    assert_eq!(stats.profile_window(&b).unwrap().samples.len(), 5);
    assert_eq!(stats.average_for_profile(&b), Some(90));
}

fn v1_body() -> String {
    json!({
        "schema_version":1,
        "feedback":{"gpu":{"completion":{
            "model":"a","completed":100,"duration_samples":100,
            "total_work_unit_ns":10000,"total_queue_wait_ms":7,"last_work_unit_ns":100,
            "profile":profile("a"),"profile_duration_samples":100,
            "profile_total_work_unit_ns":10000
        }},"cpu":{}}
    })
    .to_string()
}

#[test]
fn a_v1_snapshot_migrates_keeps_aggregates_and_is_backed_up() {
    let store = crate::store::Store::in_memory().unwrap();
    let old = v1_body();
    store.put_snapshot(super::FEEDBACK_SNAPSHOT, &old).unwrap();
    let loaded = super::load_feedback(&store).unwrap();
    let stats = &loaded.gpu[&TaskKind::Completion];
    assert_eq!(stats.completed, 100);
    assert_eq!(stats.duration_samples, 100);
    assert_eq!(stats.total_work_unit_ns, 10000);
    assert_eq!(stats.total_queue_wait_ms, 7);
    // Diagnostics only: no profile evidence is invented from the v1 running average.
    assert!(stats.profile.is_none());
    assert_eq!(stats.average_for_profile(&profile("a")), None);
    assert_eq!(
        store
            .snapshot(super::FEEDBACK_V1_BACKUP)
            .unwrap()
            .as_deref(),
        Some(old.as_str())
    );
    // Until something is learned the stored body stays schema 1; the next persist writes v2
    // and the backup keeps the original.
    super::persist_feedback(&store, &loaded).unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().unwrap()).unwrap();
    assert_eq!(written["schema_version"], 2);
    assert_eq!(written["feedback"]["gpu"]["completion"]["completed"], 100);
    assert_eq!(
        store
            .snapshot(super::FEEDBACK_V1_BACKUP)
            .unwrap()
            .as_deref(),
        Some(old.as_str())
    );
    assert_eq!(super::load_feedback(&store).unwrap().gpu.len(), 1);
}

#[test]
fn unknown_or_corrupt_feedback_starts_empty_and_is_not_overwritten() {
    for body in [
        "not-json",
        r#"{"schema_version":999,"feedback":{"gpu":{},"cpu":{}}}"#,
    ] {
        let store = crate::store::Store::in_memory().unwrap();
        store.put_snapshot(super::FEEDBACK_SNAPSHOT, body).unwrap();
        let (feedback, warning) = super::load_feedback_or_empty(&store);
        assert!(feedback.gpu.is_empty() && feedback.cpu.is_empty());
        let warning = warning.expect("a warning names the skipped snapshot");
        assert!(warning.contains("placement_feedback"), "{warning}");
        assert!(super::persist_feedback(&store, &feedback).is_err());
        assert_eq!(
            store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().as_deref(),
            Some(body)
        );
    }
}

#[test]
fn phase_estimates_require_independent_valid_observations() {
    let a = profile("a");
    let mut stats = FeedbackStats::default();
    for index in 0..3 {
        stats.record_profile_with_phases(
            &a,
            200,
            super::PhaseSample {
                queue_wait_ns: Some(0),
                upstream_setup_ns: (index < 2).then_some(10),
                prefill_ns_per_token: Some(100),
                decode_ns_per_token: Some(200),
            },
        );
    }
    assert_eq!(
        stats.phases_for_profile(&a),
        Some(super::PhaseEstimate {
            queue_wait_ns: Some(0),
            upstream_setup_ns: None,
            prefill_ns_per_token: Some(100),
            decode_ns_per_token: Some(200),
        })
    );
    stats.record_profile_with_phases(
        &a,
        200,
        super::PhaseSample {
            upstream_setup_ns: Some(10),
            prefill_ns_per_token: Some(0),
            ..super::PhaseSample::default()
        },
    );
    let estimate = stats.phases_for_profile(&a).unwrap();
    assert_eq!(estimate.upstream_setup_ns, Some(10));
    assert_eq!(estimate.prefill_ns_per_token, Some(100));
    assert_eq!(stats.phases_for_profile(&profile("unknown")), None);
}

#[test]
fn history_bounds_keep_the_newest_samples_and_profiles() {
    let a = profile("a");
    let mut stats = FeedbackStats::default();
    for index in 1..=40 {
        stats.record_profile(&a, index);
        let window = stats.profiles.get_mut(&super::profile_key(&a)).unwrap();
        let sample = window
            .samples
            .values_mut()
            .max_by_key(|sample| sample.observed_at_ns)
            .unwrap();
        sample.observed_at_ns = index;
    }
    assert_eq!(stats.profile_window(&a).unwrap().samples.len(), 32);
    assert_eq!(stats.average_for_profile(&a), Some(24)); // mean of 9..=40
    for index in 1..=40 {
        let p = profile(&format!("model-{index:02}"));
        stats.record_profile(&p, 100);
        for sample in stats
            .profiles
            .get_mut(&super::profile_key(&p))
            .unwrap()
            .samples
            .values_mut()
        {
            sample.observed_at_ns = 100 + index;
        }
    }
    assert_eq!(stats.profiles.len(), super::MAX_PROFILE_WINDOWS);
    assert!(stats.profile_window(&a).is_none());
    assert!(stats.profile_window(&profile("model-08")).is_none());
    assert!(stats.profile_window(&profile("model-09")).is_some());
    assert!(stats.profile_window(&profile("model-40")).is_some());
}

#[test]
fn snapshot_merge_is_order_independent_even_for_conflicting_sample_ids() {
    let a = profile("a");
    let mut first = FeedbackStats::default();
    learn(&mut first, &a, 100);
    let mut second = first.clone();
    let window = second.profiles.get_mut(&super::profile_key(&a)).unwrap();
    window.samples.values_mut().next().unwrap().work_unit_ns = 200;
    let forward = super::merge_stats(&first, &second);
    let reverse = super::merge_stats(&second, &first);
    assert_eq!(
        serde_json::to_value(&forward).unwrap(),
        serde_json::to_value(&reverse).unwrap()
    );
    let samples = &forward.profile_window(&a).unwrap().samples;
    assert_eq!(samples.len(), 1);
    assert_eq!(samples.values().next().unwrap().work_unit_ns, 200);
}

#[test]
fn persistence_does_not_replace_corrupt_or_unknown_version_evidence() {
    let store = crate::store::Store::in_memory().unwrap();
    for body in [
        "not-json",
        r#"{"schema_version":999,"feedback":{"gpu":{},"cpu":{}}}"#,
    ] {
        store.put_snapshot(super::FEEDBACK_SNAPSHOT, body).unwrap();
        assert!(super::persist_feedback(&store, &super::PlacementFeedback::default()).is_err());
        assert_eq!(
            store.snapshot(super::FEEDBACK_SNAPSHOT).unwrap().as_deref(),
            Some(body)
        );
    }
}

#[test]
fn feedback_metric_uses_the_right_ollama_counters_for_every_task() {
    let response = json!({
        "total_duration": 1_200,
        "prompt_eval_count": 12,
        "eval_duration": 700,
        "eval_count": 7,
    });
    let tasks = [
        TaskKind::Completion,
        TaskKind::Coding,
        TaskKind::CodeRepair,
        TaskKind::Tools,
        TaskKind::Browser,
        TaskKind::Vision,
        TaskKind::Embedding,
        TaskKind::LongContext,
    ];
    for task in tasks {
        assert_eq!(
            feedback_work_unit_ns(task, &response),
            Some(100),
            "wrong normalization counters for {task:?}"
        );
    }
}
