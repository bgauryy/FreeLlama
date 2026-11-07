//! Prompt-free completion estimates from qualified Ollama phase metrics.
//!
//! These estimates cover an idle, compatible resident runner. Busy queues and cold loads stay
//! unknown; an average historical queue delay is not a prediction of the current queue. Ollama
//! continues to own token scheduling, cache reuse and physical execution.

use serde::Serialize;
use serde_json::{Value, json};

use super::super::{
    Backend, TaskKind,
    feedback::{PhaseEstimate, PhaseSample},
};

#[derive(Debug, Clone, Serialize)]
pub(super) struct CandidatePlan {
    pub(super) model: String,
    pub(super) backend: Backend,
    pub(super) profile_id: Option<String>,
    pub(super) status: &'static str,
    pub(super) estimated_input_tokens: Option<u64>,
    pub(super) estimated_output_tokens: Option<u64>,
    pub(super) queue_ns: Option<u128>,
    pub(super) upstream_setup_ns: Option<u128>,
    pub(super) prefill_ns: Option<u128>,
    pub(super) decode_ns: Option<u128>,
    pub(super) estimated_completion_ns: Option<u128>,
    pub(super) cache_assumption: &'static str,
    pub(super) snapshot_only: bool,
}

impl CandidatePlan {
    pub(super) fn unknown(model: &str, backend: Backend, status: &'static str) -> Self {
        Self {
            model: model.to_owned(),
            backend,
            profile_id: None,
            status,
            estimated_input_tokens: None,
            estimated_output_tokens: None,
            queue_ns: None,
            upstream_setup_ns: None,
            prefill_ns: None,
            decode_ns: None,
            estimated_completion_ns: None,
            cache_assumption: "full_prefill_no_speculative_cache_credit",
            snapshot_only: true,
        }
    }

    pub(super) fn estimate(
        &mut self,
        profile_id: &str,
        phases: &PhaseEstimate,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        self.profile_id = Some(profile_id.to_owned());
        self.estimated_input_tokens = Some(input_tokens);
        self.estimated_output_tokens = Some(output_tokens);
        self.queue_ns = Some(0);
        self.upstream_setup_ns = phases.upstream_setup_ns;
        self.prefill_ns = scaled(phases.prefill_ns_per_token, input_tokens);
        self.decode_ns = scaled(phases.decode_ns_per_token, output_tokens);
        self.estimated_completion_ns = self
            .upstream_setup_ns
            .zip(self.prefill_ns)
            .zip(self.decode_ns)
            .and_then(|((setup, prefill), decode)| setup.checked_add(prefill)?.checked_add(decode));
        self.status = if self.estimated_completion_ns.is_some() {
            "qualified_resident_idle_estimate"
        } else {
            "insufficient_phase_history"
        };
    }
}

fn scaled(rate: Option<u128>, tokens: u64) -> Option<u128> {
    if tokens == 0 {
        Some(0)
    } else {
        rate?.checked_mul(u128::from(tokens))
    }
}

/// Only a material advantage changes placement. Missing evidence is never treated as zero.
pub(super) fn preferred(plans: &[CandidatePlan]) -> Option<Backend> {
    let gpu = plans
        .iter()
        .find(|plan| plan.backend == Backend::Gpu)?
        .estimated_completion_ns?;
    let cpu = plans
        .iter()
        .find(|plan| plan.backend == Backend::Cpu)?
        .estimated_completion_ns?;
    let improves = |a: u128, b: u128| {
        a.checked_mul(100)
            .zip(b.checked_mul(90))
            .is_some_and(|(a, b)| a < b)
    };
    if improves(cpu, gpu) {
        Some(Backend::Cpu)
    } else if improves(gpu, cpu) {
        Some(Backend::Gpu)
    } else {
        None
    }
}

pub(super) fn receipt(
    plans: &[CandidatePlan],
    applied: bool,
    payload_rejections: &[Value],
) -> Value {
    json!({
        "mode":"qualified_resident_idle", "snapshot_only":true, "applied":applied,
        "minimum_improvement_percent":10, "candidates":plans,
        "payload_fit_rejections":payload_rejections,
        "fallback":"existing_eligibility_affinity_and_placement_policy",
        "limitations":"estimates_not_measured_savings; cold_load_and_busy_queue_costs_unknown; no_speculative_prefix_credit"
    })
}

fn rate(duration: Option<u64>, tokens: Option<u64>) -> Option<u64> {
    duration.zip(tokens).and_then(|(duration, tokens)| {
        (tokens > 0 && duration > 0).then(|| duration.div_ceil(tokens))
    })
}

/// The upstream timer is setup/scheduling, not proof of disk I/O. Cached-token counters, when
/// present, prevent cache hits from being learned as artificially cheap full prompt processing.
pub(super) fn phase_sample(task: TaskKind, response: &Value, queue_wait_ms: u128) -> PhaseSample {
    let setup = response["load_duration"].as_u64();
    let input = response["prompt_eval_count"].as_u64();
    let prefill = if matches!(task, TaskKind::Embedding) {
        rate(
            response["total_duration"]
                .as_u64()
                .zip(setup)
                .and_then(|(total, setup)| total.checked_sub(setup)),
            input,
        )
    } else {
        let uncached = input
            .zip(response["prompt_eval_cached_count"].as_u64())
            .and_then(|(total, cached)| total.checked_sub(cached));
        rate(response["prompt_eval_duration"].as_u64(), uncached)
    };
    PhaseSample {
        queue_wait_ns: queue_wait_ms
            .checked_mul(1_000_000)
            .and_then(|value| u64::try_from(value).ok()),
        upstream_setup_ns: setup,
        prefill_ns_per_token: prefill,
        decode_ns_per_token: rate(
            response["eval_duration"].as_u64(),
            response["eval_count"].as_u64(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(
        backend: Backend,
        prefill: u128,
        decode: u128,
        input: u64,
        output: u64,
    ) -> CandidatePlan {
        let mut plan = CandidatePlan::unknown("fixture", backend, "unknown");
        plan.estimate(
            "exact-profile",
            &PhaseEstimate {
                queue_wait_ns: Some(999_000_000),
                upstream_setup_ns: Some(1_000),
                prefill_ns_per_token: Some(prefill),
                decode_ns_per_token: Some(decode),
            },
            input,
            output,
        );
        plan
    }

    #[test]
    fn prompt_work_can_outweigh_faster_decode_without_reusing_historical_queue_delay() {
        let gpu = plan(Backend::Gpu, 100, 10, 1000, 10);
        let cpu = plan(Backend::Cpu, 10, 100, 1000, 10);
        assert_eq!(preferred(&[gpu.clone(), cpu]), Some(Backend::Cpu));
        assert_eq!(gpu.queue_ns, Some(0));
        let gpu = plan(Backend::Gpu, 100, 10, 10, 1000);
        let cpu = plan(Backend::Cpu, 10, 100, 10, 1000);
        assert_eq!(preferred(&[gpu, cpu]), Some(Backend::Gpu));
    }

    #[test]
    fn unknown_busy_or_cold_candidate_does_not_become_a_zero_cost_route() {
        let gpu = plan(Backend::Gpu, 100, 10, 1000, 10);
        let cpu = CandidatePlan::unknown("cold", Backend::Cpu, "not_compatible_resident_idle");
        assert_eq!(preferred(&[gpu, cpu]), None);
    }

    #[test]
    fn close_predictions_keep_existing_policy_and_overflow_is_unknown() {
        let gpu = plan(Backend::Gpu, 100, 100, 100, 100);
        let cpu = plan(Backend::Cpu, 95, 95, 100, 100);
        assert_eq!(preferred(&[gpu, cpu]), None);
        let overflow = plan(Backend::Cpu, u128::MAX, 1, 2, 1);
        assert_eq!(overflow.estimated_completion_ns, None);
    }

    #[test]
    fn cache_hits_are_not_full_prefill_training_and_missing_counters_are_unknown() {
        let sample = phase_sample(
            TaskKind::Completion,
            &json!({
                "load_duration":0,"prompt_eval_count":100,"prompt_eval_cached_count":90,
                "prompt_eval_duration":1000,"eval_duration":500,"eval_count":5
            }),
            7,
        );
        assert_eq!(sample.prefill_ns_per_token, Some(100));
        assert_eq!(sample.decode_ns_per_token, Some(100));
        assert_eq!(sample.queue_wait_ns, Some(7_000_000));
        for cached in [Value::Null, json!(101), json!(100)] {
            let sample = phase_sample(
                TaskKind::Completion,
                &json!({
                    "prompt_eval_count":100,"prompt_eval_cached_count":cached,"prompt_eval_duration":1000
                }),
                0,
            );
            assert_eq!(sample.prefill_ns_per_token, None);
        }
    }

    #[test]
    fn embedding_estimate_separates_setup_and_needs_no_decode_samples() {
        let sample = phase_sample(
            TaskKind::Embedding,
            &json!({
                "total_duration":5000,"load_duration":1000,"prompt_eval_count":20
            }),
            0,
        );
        assert_eq!(sample.prefill_ns_per_token, Some(200));
        let mut candidate = CandidatePlan::unknown("embed", Backend::Cpu, "unknown");
        candidate.estimate(
            "profile",
            &PhaseEstimate {
                queue_wait_ns: None,
                upstream_setup_ns: Some(1000),
                prefill_ns_per_token: Some(200),
                decode_ns_per_token: None,
            },
            20,
            0,
        );
        assert_eq!(candidate.estimated_completion_ns, Some(5000));
    }

    #[test]
    fn ollama_phase_observations_enable_a_plan_only_after_qualified_history() {
        use crate::platform::{
            feedback::FeedbackStats,
            runtime::{ExecutionProfile, ThroughputMetricKind},
        };
        let profile = ExecutionProfile {
            id: "runtime-controls".into(),
            task: TaskKind::Completion,
            model: "fixture".into(),
            digest: "exact-revision".into(),
            context_tokens: 4096,
            processor: "gpu".into(),
            metric_kind: ThroughputMetricKind::OutputTokensPerSecond,
        };
        let mut stats = FeedbackStats::default();
        let response = json!({"load_duration":1000,"prompt_eval_count":100,
            "prompt_eval_cached_count":90,"prompt_eval_duration":1000,
            "eval_duration":500,"eval_count":5});
        for count in 1..=3 {
            stats.record("fixture", Some(100), 0);
            stats.record_profile_with_phases(
                &profile,
                100,
                phase_sample(TaskKind::Completion, &response, 0),
            );
            let mut candidate = CandidatePlan::unknown("fixture", Backend::Gpu, "unknown");
            candidate.estimate(
                &profile.id,
                &stats.phases_for_profile(&profile).unwrap(),
                1000,
                20,
            );
            assert_eq!(
                candidate.estimated_completion_ns,
                (count == 3).then_some(103_000)
            );
        }
        let changed = ExecutionProfile {
            digest: "other-revision".into(),
            ..profile
        };
        assert_eq!(stats.phases_for_profile(&changed), None);
    }
}
