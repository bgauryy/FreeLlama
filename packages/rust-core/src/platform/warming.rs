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
use std::time::Duration;

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
        let reuse = usage.recent_uses * super::float(self.reuse_gain_seconds);
        let load = usage.load_ms.unwrap_or_default() / 1000.0 * self.load_multiplier;
        let pressure = if holding { self.pressure_factor } else { 1.0 };
        let seconds = ((super::float(self.base_seconds) + reuse + load) * pressure)
            .clamp(
                super::float(self.min_seconds),
                super::float(self.max_seconds),
            )
            .round();
        // Finite, rounded, and clamped to `[min_seconds, max_seconds]` (validated), so the checked
        // conversion is exact; the fallback is unreachable.
        Duration::try_from_secs_f64(seconds).map_or(self.max_seconds, |duration| duration.as_secs())
    }
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
    /// Chat messages to evaluate into the runner's prompt cache while loading, such as a long
    /// system prompt: later requests that start with the same messages skip evaluating them.
    messages: Option<Vec<Value>>,
}
impl WarmInput {
    fn into_task(self) -> Result<TaskInput, ApiError> {
        if self.model.trim().is_empty() {
            return Err(ApiError::bad_request(
                "warm requires an installed model tag",
            ));
        }
        let mut value = serde_json::to_value(self).expect("warm request serializes");
        value
            .as_object_mut()
            .expect("object")
            .retain(|_, value| !value.is_null());
        let mut input: TaskInput = serde_json::from_value(value).map_err(ApiError::bad_request)?;
        input.operation = super::ManagedOperation::Warm;
        Ok(input)
    }
}
/// A background-able warm of `model`, as the preloader runs it.
pub(super) fn preload_task(
    model: String,
    context_tokens: Option<u64>,
    keep_alive: Option<String>,
    messages: Vec<Value>,
    priority: super::TaskPriority,
) -> Result<TaskInput, ApiError> {
    WarmInput {
        model,
        task: None,
        context_tokens,
        execution_preference: None,
        min_placement_evidence: None,
        priority: Some(priority),
        keep_alive,
        max_wait_seconds: None,
        timeout_seconds: None,
        defer: None,
        messages: (!messages.is_empty()).then_some(messages),
    }
    .into_task()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UnloadInput {
    model: String,
    max_wait_seconds: Option<u64>,
}

/// Unload a resident model once the tasks using it finish. Unlike `warm` with `keep_alive: 0`,
/// a model that is not resident is reported, never loaded first.
pub(super) async fn unload(
    State(state): State<PlatformState>,
    input: Result<Json<UnloadInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    let model = input.model.trim();
    if model.is_empty() {
        return Err(ApiError::bad_request("unload requires a model tag"));
    }
    let wait = input.max_wait_seconds.map_or_else(
        || state.tunables().max_queue_wait(),
        |seconds| Duration::from_secs(seconds.max(1)),
    );
    Ok(Json(
        execution::unload_resident_model(&state, model, wait)
            .await?
            .unwrap_or_else(|| {
                json!({
                    "model": model,
                    "backend": execution::execution_target_name(&state, model),
                    "unloaded": false,
                    "status": "not_resident",
                })
            }),
    ))
}

pub(super) async fn warm(
    State(state): State<PlatformState>,
    caller: Option<axum::Extension<crate::proxy::Caller>>,
    input: Result<Json<WarmInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    let mut input = input;
    let unload = execution::requests_immediate_unload(input.keep_alive.as_deref())
        && input.messages.as_ref().is_none_or(Vec::is_empty)
        && input.defer != Some(true);
    // `keep_alive: 0` on a resident model with nothing to evaluate is an unload, not a load: it
    // needs no task, route or eligibility, only the model's name. A model that is not resident
    // keeps the documented observe-then-unload probe below.
    if unload && !input.model.trim().is_empty() {
        let wait = input.max_wait_seconds.map_or_else(
            || state.tunables().max_queue_wait(),
            |seconds| Duration::from_secs(seconds.max(1)),
        );
        if let Some(receipt) =
            execution::unload_resident_model(&state, input.model.trim(), wait).await?
        {
            return Ok(Json(receipt).into_response());
        }
    }
    // Without a task, warm the model as what it is: an embedding-only model as an embedding task
    // rather than refusing it as ineligible for completion.
    if input.task.is_none()
        && let Ok(models) = super::catalog::discover_models(&state).await
        && let Some(model) = models.iter().find(|model| model.name == input.model.trim())
        && model
            .capabilities
            .contains(&crate::capability::Capability::Embedding)
        && !model
            .capabilities
            .contains(&crate::capability::Capability::Completion)
    {
        input.task = Some(TaskKind::Embedding);
    }
    let mut input = input.into_task()?;
    input.client = caller.map(|axum::Extension(caller)| caller.0.to_string());
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
    let policy = state.tunables().warming.clone();
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
    json!({"requested":true,"loaded":observation["resident"],"load_response_validated":true,"residency_source":source,"model_shared_across_scopes":true,"kv_transfer_between_models":false})
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
