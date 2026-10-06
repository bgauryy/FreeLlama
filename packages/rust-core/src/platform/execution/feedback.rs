//! Completion feedback: task records, adaptive throughput metrics and runtime metrics.

use std::time::Instant;

use anyhow::Result;
use axum::Json;
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{PlatformState, TaskKind, TaskPriority, resources};
use super::ExecutionTarget;

pub(crate) fn record_completion(
    state: &PlatformState,
    mut record: super::super::telemetry::TaskRecord,
    started: Instant,
) {
    record.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if record.outcome == "ok" {
        state
            .activity
            .completed(&record.backend, &record.model, record.load_ms);
    }
    state.telemetry.record_task(record);
}

/// Usage-ledger row for a finished managed call, successful or not.
pub(crate) fn task_record(
    result: &Result<Json<Value>, ApiError>,
    started: Instant,
    task: &str,
    priority: TaskPriority,
    requested_model: Option<&str>,
) -> super::super::telemetry::TaskRecord {
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let priority = match priority {
        TaskPriority::Interactive => "interactive",
        TaskPriority::Normal => "normal",
        TaskPriority::Background => "background",
    };
    let mut record = super::super::telemetry::TaskRecord {
        ts: super::super::telemetry::now_seconds(),
        model: requested_model.unwrap_or("unrouted").to_owned(),
        backend: "none".into(),
        task: task.to_owned(),
        priority: priority.to_owned(),
        outcome: "ok".into(),
        status: 200,
        prompt_tokens: 0,
        output_tokens: 0,
        duration_ms,
        queue_wait_ms: 0,
        load_ms: 0,
        output_tokens_per_second: None,
    };
    match result {
        Ok(Json(value)) => {
            if let Some(model) = value["route"]["selected_model"].as_str() {
                model.clone_into(&mut record.model);
            }
            if let Some(backend) = value["execution"]["placement"].as_str() {
                backend.clone_into(&mut record.backend);
            }
            let metrics = &value["metrics"];
            record.prompt_tokens = metrics["prompt_tokens"].as_u64().unwrap_or(0);
            record.output_tokens = metrics["output_tokens"].as_u64().unwrap_or(0);
            record.load_ms = metrics["load_duration_ns"].as_u64().unwrap_or(0) / 1_000_000;
            record.output_tokens_per_second = metrics["output_tokens_per_second"].as_f64();
            record.queue_wait_ms = value["admission"]["total_wait_ms"].as_u64().unwrap_or(0);
        }
        Err(error) => {
            record.outcome = "error".into();
            record.status = error.status().as_u16();
        }
    }
    record
}

/// Feed adaptive concurrency with comparable observed feedback or a backend safety signal.
pub(crate) async fn observe_completion(
    state: &PlatformState,
    execution: &ExecutionTarget,
    failed: bool,
    profile: Option<&super::super::runtime::ExecutionProfile>,
    metric: Option<super::super::runtime::ThroughputMetric>,
) {
    if !super::super::monitor::adaptive_applies(state, execution.placement) {
        state
            .adaptive_for(execution.placement)
            .reset_healthy_streak();
        return;
    }
    let observation = state.resources.snapshot().await.observation;
    let memory_pressure = matches!(
        observation.memory_pressure,
        Some(resources::MemoryPressure::Warning | resources::MemoryPressure::Critical)
    ) || observation
        .memory_psi_some_avg10
        .is_some_and(|stall| stall >= ADAPTIVE_PSI_THRESHOLD);
    let change = state.adaptive_for(execution.placement).observe(
        &execution.admission,
        &super::super::runtime::Completion {
            profile,
            failed,
            memory_pressure,
            metric: metric.filter(|_| profile.is_some()),
        },
    );
    super::super::runtime::note_limit_change(
        &state.telemetry,
        execution.placement.as_str(),
        change,
    );
}

/// PSI memory `some avg10` (percent of the last 10s with a task stalled on memory) at which a
/// completion counts as "under pressure" for adaptive concurrency.
const ADAPTIVE_PSI_THRESHOLD: f64 = 10.0;

/// Normalize unlike prompt sizes before backend feedback compares them. Generation speed uses
/// decode nanoseconds per output token; embeddings use total nanoseconds per input token because
/// Ollama does not report a separate embedding-evaluation duration.
pub(crate) fn feedback_work_unit_ns(task: TaskKind, response: &Value) -> Option<u64> {
    let (duration, units) = if matches!(task, TaskKind::Embedding) {
        (
            response.get("total_duration").and_then(Value::as_u64),
            response.get("prompt_eval_count").and_then(Value::as_u64),
        )
    } else {
        (
            response.get("eval_duration").and_then(Value::as_u64),
            response.get("eval_count").and_then(Value::as_u64),
        )
    };
    match (duration, units) {
        (Some(duration), Some(units)) if duration > 0 && units > 0 && duration / units > 0 => {
            Some(duration / units)
        }
        _ => None,
    }
}

pub(crate) fn adaptive_throughput_metric(
    task: TaskKind,
    response: &Value,
) -> Option<super::super::runtime::ThroughputMetric> {
    let rate = if matches!(task, TaskKind::Embedding) {
        tokens_per_second(Some(1), Some(feedback_work_unit_ns(task, response)?))?
    } else {
        runtime_metrics(response)["output_tokens_per_second"].as_f64()?
    };
    (rate.is_finite() && rate > 0.0).then_some(super::super::runtime::ThroughputMetric {
        kind: super::super::runtime::ThroughputMetricKind::for_task(task),
        tokens_per_second: rate,
    })
}

/// Extract prompt-free performance fields from an Ollama response.
#[must_use]
pub fn runtime_metrics(response: &Value) -> Value {
    let prompt_count = response.get("prompt_eval_count").and_then(Value::as_u64);
    let cached_count = response
        .get("prompt_eval_cached_count")
        .and_then(Value::as_u64)
        .filter(|cached| prompt_count.is_some_and(|total| *cached <= total));
    let uncached_count = prompt_count
        .zip(cached_count)
        .map(|(total, cached)| total - cached);
    let prompt_duration = response.get("prompt_eval_duration").and_then(Value::as_u64);
    let output_count = response.get("eval_count").and_then(Value::as_u64);
    let output_duration = response.get("eval_duration").and_then(Value::as_u64);
    json!({
        "total_duration_ns": response.get("total_duration").and_then(Value::as_u64),
        "load_duration_ns": response.get("load_duration").and_then(Value::as_u64),
        "prompt_tokens": prompt_count,
        "cached_prompt_tokens": cached_count,
        "uncached_prompt_tokens": uncached_count,
        "prompt_duration_ns": prompt_duration,
        "prompt_tokens_per_second": tokens_per_second(uncached_count, prompt_duration),
        "output_tokens": output_count,
        "output_duration_ns": output_duration,
        "output_tokens_per_second": tokens_per_second(output_count, output_duration),
    })
}

pub(crate) fn tokens_per_second(count: Option<u64>, duration_ns: Option<u64>) -> Option<f64> {
    let (Some(count), Some(duration_ns)) = (count, duration_ns) else {
        return None;
    };
    if duration_ns == 0 {
        return None;
    }
    Some(super::super::float(count) * 1_000_000_000.0 / super::super::float(duration_ns))
}

#[cfg(test)]
mod execution_profile_tests {
    use super::super::super::runtime::{
        AdaptiveLimiter, Completion, ThroughputMetric, ThroughputMetricKind,
    };
    use super::super::select::{ProfileEvidence, profile_from_observations};
    use super::*;
    use crate::platform::admission::AdmissionPool;
    use std::collections::BTreeMap;

    struct Observations {
        task: TaskKind,
        body: Value,
        placement: Value,
        start: Value,
        end: Value,
        settings: Option<Value>,
        end_settings: Option<Value>,
        digest: String,
        upstream: String,
    }
    impl Observations {
        fn new() -> Self {
            let settings = super::super::super::ollama_env::resolve(
                Some(&BTreeMap::new()),
                |_| None,
                |_| None,
                "observed fixture pid 42".into(),
            )
            .comparable_process_settings();
            let admission = json!({"in_flight":1,"active_units":2,"slots_total":4,"slots_ceiling":4,"admitted":1,"released":0,"policy_epoch":0});
            Self {
                task: TaskKind::Completion,
                body: json!({"options":{"num_ctx":4096,"num_predict":512},"think":false,"prompt":"private prompt"}),
                placement: json!({"status":"verified","processor":"gpu","digest":"revision-a","context_length":4096}),
                start: admission.clone(),
                end: admission,
                end_settings: settings.clone(),
                settings,
                digest: "revision-a".into(),
                upstream: "http://127.0.0.1:11434".into(),
            }
        }
        fn profile(&self) -> Result<super::super::super::runtime::ExecutionProfile, &'static str> {
            profile_from_observations(ProfileEvidence {
                task: self.task,
                model: "model:latest",
                catalog_digest: Some(&self.digest),
                upstream: &self.upstream,
                body: &self.body,
                placement: &self.placement,
                dispatch_admission: &self.start,
                completion_admission: &self.end,
                dispatch_settings: self.settings.as_ref(),
                completion_settings: self.end_settings.as_ref(),
                warm: true,
            })
        }
    }

    fn trained_limiter(
        profile: &super::super::super::runtime::ExecutionProfile,
    ) -> (AdmissionPool, AdaptiveLimiter) {
        let pool = AdmissionPool::new(4, 4);
        let limiter = AdaptiveLimiter::default();
        for _ in 0..3 {
            limiter.observe(
                &pool,
                &Completion {
                    profile: Some(profile),
                    failed: false,
                    memory_pressure: false,
                    metric: Some(ThroughputMetric {
                        kind: ThroughputMetricKind::OutputTokensPerSecond,
                        tokens_per_second: 100.0,
                    }),
                },
            );
        }
        (pool, limiter)
    }

    #[test]
    fn comparable_profile_accumulates_but_distinct_execution_controls_do_not_mix() {
        let original = Observations::new().profile().unwrap();
        let changes: &[fn(&mut Observations)] = &[
            |facts| {
                facts.body["options"]["num_ctx"] = json!(8192);
                facts.placement["context_length"] = json!(8192);
            },
            |facts| {
                facts.digest = "revision-b".into();
                facts.placement["digest"] = json!("revision-b");
            },
            |facts| {
                facts.placement["processor"] = json!("cpu");
                facts.upstream = "http://127.0.0.1:11436".into();
            },
            |facts| {
                facts.body["options"]["num_thread"] = json!(1);
            },
            |facts| {
                facts.body["options"]["temperature"] = json!(0.8);
            },
            |facts| {
                facts.body["think"] = json!(true);
            },
            |facts| {
                facts.body["tools"] = json!([{"type":"function","function":{"name":"inspect"}}]);
            },
            |facts| {
                facts.body["format"] = json!({"type":"object"});
            },
            |facts| {
                facts.settings.as_mut().unwrap()["settings"]["OLLAMA_FLASH_ATTENTION"]["value"] =
                    json!("1");
                facts.end_settings = facts.settings.clone();
            },
            |facts| {
                facts.start["slots_total"] = json!(2);
                facts.end = facts.start.clone();
            },
            |facts| {
                facts.start["active_units"] = json!(3);
                facts.end = facts.start.clone();
            },
        ];
        for change in changes {
            let (pool, limiter) = trained_limiter(&original);
            let baseline = limiter.receipt(&pool, true);
            assert_eq!(baseline["profile_windows"]["completion"]["samples"], 3);
            let mut facts = Observations::new();
            change(&mut facts);
            let distinct = facts.profile().unwrap();
            assert_ne!(distinct.id, original.id);
            assert_eq!(
                limiter.observe(
                    &pool,
                    &Completion {
                        profile: Some(&distinct),
                        failed: false,
                        memory_pressure: false,
                        metric: Some(ThroughputMetric {
                            kind: ThroughputMetricKind::OutputTokensPerSecond,
                            tokens_per_second: 10.0
                        })
                    }
                ),
                None
            );
            assert_eq!(
                pool.total(),
                4,
                "a slow distinct profile must not reduce the limit"
            );
            assert_eq!(
                limiter.receipt(&pool, true)["current_profile_count"],
                1,
                "only one current task window is retained"
            );
        }
        let (pool, limiter) = trained_limiter(&original);
        assert_eq!(
            limiter.observe(
                &pool,
                &Completion {
                    profile: Some(&original),
                    failed: false,
                    memory_pressure: false,
                    metric: Some(ThroughputMetric {
                        kind: ThroughputMetricKind::OutputTokensPerSecond,
                        tokens_per_second: 10.0
                    })
                }
            ),
            Some(("decrease", 2))
        );
        let mut same = Observations::new();
        same.body["prompt"] = json!("different private content");
        assert_eq!(
            same.profile().unwrap(),
            original,
            "raw prompts must not enter profile identity"
        );
    }

    #[test]
    fn comparable_embedding_input_rate_can_recover_a_reduced_limit() {
        let pool = AdmissionPool::new(4, 4);
        pool.set_limit(2);
        let limiter = AdaptiveLimiter::default();
        let mut facts = Observations::new();
        facts.task = TaskKind::Embedding;
        facts.body = json!({"options":{"num_ctx":4096}});
        let profile = facts.profile().unwrap();
        assert_eq!(
            profile.metric_kind,
            ThroughputMetricKind::InputTokensPerSecond
        );
        assert_ne!(profile.id, Observations::new().profile().unwrap().id);
        let response = json!({"prompt_eval_count":20,"total_duration":200_000_000});
        let completion = Completion {
            profile: Some(&profile),
            failed: false,
            memory_pressure: false,
            metric: adaptive_throughput_metric(TaskKind::Embedding, &response),
        };
        for _ in 0..5 {
            limiter.observe(&pool, &completion);
        }
        assert_eq!(
            pool.total(),
            3,
            "comparable positive embedding input-rate completions must recover"
        );
        let receipt = limiter.receipt(&pool, true);
        assert_eq!(
            receipt["profile_windows"]["embedding"]["metric_kind"],
            "input_tokens_per_second"
        );
        assert_eq!(receipt["profile_windows"]["embedding"]["rate"], 100.0);
        assert_eq!(receipt["baseline_output_tokens_per_second"], json!({}));
        let slow = json!({"prompt_eval_count":20,"total_duration":2_000_000_000_u64});
        assert_eq!(
            limiter.observe(
                &pool,
                &Completion {
                    metric: adaptive_throughput_metric(TaskKind::Embedding, &slow),
                    ..completion
                }
            ),
            Some(("decrease", 1))
        );
        limiter.observe(&pool, &completion);
        limiter.observe(
            &pool,
            &Completion {
                metric: Some(ThroughputMetric {
                    kind: ThroughputMetricKind::OutputTokensPerSecond,
                    tokens_per_second: 100.0,
                }),
                ..completion
            },
        );
        assert_eq!(
            limiter.receipt(&pool, true)["healthy_streak"],
            0,
            "unlike metric kinds cannot bridge recovery"
        );
        assert_eq!(
            limiter.receipt(&pool, true)["profile_windows"]["embedding"]["samples"],
            6,
            "metric mismatch cannot train"
        );
        assert_eq!(
            adaptive_throughput_metric(
                TaskKind::Embedding,
                &json!({"prompt_eval_count":0,"total_duration":20})
            )
            .map(|metric| metric.tokens_per_second),
            None
        );
    }

    #[test]
    fn missing_identity_and_changed_or_parallel_intervals_are_explicitly_unqualified() {
        type UnqualifiedCase = (&'static str, fn(&mut Observations));
        let cases: &[UnqualifiedCase] = &[
            ("model_revision_unknown", |facts| {
                facts.placement["digest"] = Value::Null;
            }),
            ("observed_context_unknown", |facts| {
                facts.placement["context_length"] = Value::Null;
            }),
            ("effective_context_mismatch_or_unknown", |facts| {
                facts.body["options"]
                    .as_object_mut()
                    .unwrap()
                    .remove("num_ctx");
            }),
            ("backend_process_settings_unknown", |facts| {
                facts.settings = None;
            }),
            ("backend_process_settings_changed_or_unknown", |facts| {
                facts.end_settings.as_mut().unwrap()["process"] = json!("observed fixture pid 43");
            }),
            ("admission_occupancy_or_policy_not_comparable", |facts| {
                facts.start["in_flight"] = json!(2);
                facts.end = facts.start.clone();
            }),
            ("admission_occupancy_or_policy_not_comparable", |facts| {
                facts.end["admitted"] = json!(2);
                facts.end["released"] = json!(1);
            }),
            ("admission_occupancy_or_policy_not_comparable", |facts| {
                facts.end["policy_epoch"] = json!(2);
            }),
        ];
        for (reason, change) in cases {
            let mut facts = Observations::new();
            change(&mut facts);
            assert_eq!(facts.profile(), Err(*reason));
        }
    }
}
