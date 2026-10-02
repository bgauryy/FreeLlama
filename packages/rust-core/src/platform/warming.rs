//! Explicit model loads and finite reuse-aware residency use ordinary managed execution.
use super::{ApiError, PlatformState, TaskInput, TaskKind, execution};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WarmingPolicy {
    pub min_seconds: u64,
    pub max_seconds: u64,
    pub base_seconds: u64,
    pub reuse_gain_seconds: u64,
    pub load_multiplier: f64,
    pub pressure_factor: f64,
}
impl Default for WarmingPolicy {
    fn default() -> Self {
        Self {
            min_seconds: 15,
            max_seconds: 900,
            base_seconds: 60,
            reuse_gain_seconds: 30,
            load_multiplier: 2.0,
            pressure_factor: 0.25,
        }
    }
}
impl WarmingPolicy {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.min_seconds > 0
                && self.min_seconds <= self.base_seconds
                && self.base_seconds <= self.max_seconds
                && self.load_multiplier.is_finite()
                && self.load_multiplier >= 0.0
                && self.pressure_factor.is_finite()
                && self.pressure_factor > 0.0
                && self.pressure_factor <= 1.0,
            "warming policy requires positive ordered finite durations, nonnegative load_multiplier, and pressure_factor in (0,1]"
        );
        Ok(())
    }
    fn duration(&self, usage: super::residency::Usage, holding: bool) -> u64 {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let seconds = {
            let reuse = usage.recent_uses * self.reuse_gain_seconds as f64;
            let load = usage.load_ms.unwrap_or_default() / 1000.0 * self.load_multiplier;
            let pressure = if holding { self.pressure_factor } else { 1.0 };
            ((self.base_seconds as f64 + reuse + load) * pressure)
                .clamp(self.min_seconds as f64, self.max_seconds as f64)
                .round() as u64
        };
        seconds
    }
}
#[derive(Debug, Clone, Copy, Default)]
pub(super) enum ManagedOperation {
    #[default]
    Generate,
    Warm,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WarmInput {
    model: String,
    task: Option<TaskKind>,
    context_tokens: Option<u64>,
    execution_preference: Option<super::ExecutionPreference>,
    min_placement_evidence: Option<super::PlacementEvidence>,
    priority: Option<super::TaskPriority>,
    keep_alive: Option<String>,
    max_wait_seconds: Option<u64>,
    timeout_seconds: Option<u64>,
    defer: Option<bool>,
}
impl WarmInput {
    fn into_task(self) -> Result<TaskInput, ApiError> {
        if self.model.trim().is_empty() || self.task == Some(TaskKind::Embedding) {
            return Err(ApiError::bad_request(
                "warm requires an installed model tag and a non-embedding task",
            ));
        }
        let mut value = serde_json::to_value(self).expect("warm request serializes");
        value
            .as_object_mut()
            .expect("object")
            .retain(|_, value| !value.is_null());
        let mut input: TaskInput = serde_json::from_value(value).map_err(ApiError::bad_request)?;
        input.operation = ManagedOperation::Warm;
        Ok(input)
    }
}
pub(super) async fn warm(
    State(state): State<PlatformState>,
    input: Result<Json<WarmInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    let input = input.into_task()?;
    if input.defer {
        return super::jobs::submit(state, input)
            .await
            .map(|job| (StatusCode::ACCEPTED, job).into_response());
    }
    execution::execute_task(State(state), Json(input))
        .await
        .map(IntoResponse::into_response)
}

pub(super) async fn keep_alive(
    state: &PlatformState,
    input: Option<String>,
    backend: &str,
    model: &str,
) -> (Value, Value) {
    if let Some(value) = input {
        let wire = if value == "-1" {
            json!(-1)
        } else {
            json!(value)
        };
        let receipt = json!({"mode":"explicit","value":wire,"reason":"caller_override","finite":!value.trim_start().starts_with('-'),"physical_placement":"reported_separately_in_execution_observation"});
        return (wire, receipt);
    }
    let policy = state.tunables().warming;
    let usage = state.activity.usage(backend, model);
    let pressure = state.resources.snapshot().await;
    let seconds = policy.duration(usage, pressure.holding);
    let wire = json!(format!("{seconds}s"));
    let receipt = json!({"mode":"adaptive","value":wire,"seconds":seconds,"finite":true,"reason":if pressure.holding {"host_pressure"} else if usage.recent_uses>0.0 {"recent_model_reuse_and_load_cost"} else {"initial_finite_residency"},"recent_uses":usage.recent_uses,"observed_load_ms":usage.load_ms,"pressure_observed":pressure.holding,"pressure_reasons":pressure.reasons,"bounds":{"min_seconds":policy.min_seconds,"max_seconds":policy.max_seconds},"physical_placement":"reported_separately_in_execution_observation"});
    (wire, receipt)
}

pub(super) fn completion_receipt(execution: &Value) -> Value {
    let unloading = execution["lifecycle"]["requested"] == "immediate_unload";
    let (observation, source) = if unloading {
        (
            &execution["lifecycle"]["post_unload_observation"],
            "ollama_api_ps_after_unload",
        )
    } else {
        (&execution["observation"], "ollama_api_ps_after_execution")
    };
    json!({"requested":true,"loaded":observation["status"] == "verified","load_response_validated":true,"residency_source":source,"model_shared_across_scopes":true,"kv_transfer_between_models":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reuse_load_and_pressure_are_bounded_and_operator_tunable() {
        let policy = WarmingPolicy::default();
        let usage = super::super::residency::Usage {
            recent_uses: 4.0,
            load_ms: Some(2000.0),
            ..Default::default()
        };
        assert_eq!(policy.duration(usage, false), 184);
        assert_eq!(policy.duration(usage, true), 46);
        assert_eq!(
            policy.duration(
                super::super::residency::Usage {
                    recent_uses: 1e20,
                    ..Default::default()
                },
                false
            ),
            900
        );
        let altered = WarmingPolicy {
            base_seconds: 120,
            reuse_gain_seconds: 10,
            ..policy
        };
        assert_eq!(altered.duration(usage, false), 164);
    }
}
