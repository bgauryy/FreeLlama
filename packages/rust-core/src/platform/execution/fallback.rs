//! An explicit, caller-ordered fallback chain (`fallback_models`).
//!
//! The next model runs only after the one before it was refused before anything executed: no room
//! in memory by the wait deadline, an input above its context window, a model that is not
//! installed or cannot do the task, or a backend whose circuit is open. A model that started
//! generating is never followed by another, so no work is replayed after an uncertain outcome, and
//! every attempt is reported: on success in the receipt's `fallback`, on failure in the error's.
//! While one model is tried, the models after it count as busy, so making room for an attempt
//! that is then refused never unloads the model that would have run next.

use std::collections::BTreeSet;

use axum::Json;
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::residency::ActiveModel;
use super::super::{ManagedOperation, ModelRefusal, PlatformState, TaskInput};
use super::nearest;

/// At most this many models after the first: each one may wait for memory in turn.
pub(super) const MAX_FALLBACK_MODELS: usize = 4;

/// The refusals that prove the attempt never reached a model, and so may move to the next one.
/// An error after part of the task ran (a later embedding slice) never qualifies.
pub(super) fn refused_before_execution(error: &ApiError) -> bool {
    !error.followed_execution()
        && matches!(
            error.code(),
            Some(
                "resource_admission_unavailable"
                    | "context_window_exceeded"
                    | "embedding_item_exceeds_context"
                    | "model_exceeds_host_budget"
                    | "upstream_circuit_open"
                    | ModelRefusal::NOT_INSTALLED
                    | ModelRefusal::NOT_ELIGIBLE
            )
        )
}

/// Refuse a chain that could not mean what it says before any model is touched.
pub(in crate::platform) fn validate(input: &TaskInput) -> Result<(), ApiError> {
    if input.fallback_models.is_empty() {
        return Ok(());
    }
    if input.fallback_models.len() > MAX_FALLBACK_MODELS {
        return Err(ApiError::bad_request(format!(
            "fallback_models takes at most {MAX_FALLBACK_MODELS} models"
        )));
    }
    if matches!(input.operation, ManagedOperation::Warm) {
        return Err(ApiError::bad_request(
            "fallback_models applies to tasks; a warm loads exactly the model it names",
        ));
    }
    if input.expected_model_digest.is_some()
        || input.expected_execution_backend.is_some()
        || input.expected_upstream.is_some()
    {
        return Err(ApiError::bad_request(
            "fallback_models cannot be combined with expected_model_digest, expected_execution_backend or expected_upstream: those pin one model",
        ));
    }
    let mut seen = BTreeSet::new();
    for model in &input.fallback_models {
        if model.trim().is_empty() {
            return Err(ApiError::bad_request(
                "fallback_models entries must be non-empty model names",
            ));
        }
        if !seen.insert(model.trim()) {
            return Err(ApiError::bad_request(format!(
                "fallback_models repeats {model}: each model is tried once"
            )));
        }
    }
    Ok(())
}

/// The attempts in order: the first model (explicit, a scope default, or routed when `None`), then
/// the chain without an entry that repeats it. The first model can come from a scope or a job's
/// pick the caller never saw, so a repeat is skipped rather than refused.
fn attempts_in_order(input: &TaskInput) -> Vec<Option<String>> {
    let first = input.route.model.clone();
    std::iter::once(first.clone())
        .chain(
            input
                .fallback_models
                .iter()
                .filter(|model| first.as_deref().map(str::trim) != Some(model.trim()))
                .cloned()
                .map(Some),
        )
        .collect()
}

/// Mark the models still to come as busy for one attempt; dropping the guards releases them. A
/// bare name is guarded as written and as `name:latest`, the tag Ollama would resolve it to.
fn protect(state: &PlatformState, later: &[Option<String>]) -> Vec<ActiveModel> {
    later
        .iter()
        .flatten()
        .flat_map(|name| {
            let latest = (!name.contains(':')).then(|| format!("{name}:latest"));
            std::iter::once(name.clone()).chain(latest)
        })
        .map(|name| {
            let backend = super::execution_target_name(state, &name);
            state.activity.begin(backend, &name)
        })
        .collect()
}

fn attempt(model: Option<&str>, error: &ApiError) -> Value {
    json!({
        "model": model,
        "code": error.code(),
        "error": error.message(),
    })
}

/// Run the task, moving down `fallback_models` on a refusal that proves nothing executed.
pub(super) async fn run(state: PlatformState, input: TaskInput) -> Result<Json<Value>, ApiError> {
    validate(&input)?;
    if input.fallback_models.is_empty() {
        return nearest::run(state, input).await;
    }
    let chain = attempts_in_order(&input);
    let mut attempts = Vec::new();
    for (index, model) in chain.iter().enumerate() {
        let mut next = input.clone();
        next.route.model.clone_from(model);
        next.fallback_models.clear();
        let guards = protect(&state, &chain[index + 1..]);
        let outcome = nearest::run(state.clone(), next).await;
        drop(guards);
        match outcome {
            Ok(Json(mut receipt)) => {
                receipt["fallback"] = json!({
                    "used": index > 0,
                    "attempts": attempts,
                    "selected": receipt["route"]["selected_model"]
                        .as_str()
                        .or(model.as_deref()),
                });
                return Ok(Json(receipt));
            }
            Err(error) if refused_before_execution(&error) && index + 1 < chain.len() => {
                attempts.push(attempt(model.as_deref(), &error));
            }
            Err(error) => {
                attempts.push(attempt(model.as_deref(), &error));
                let exhausted = refused_before_execution(&error);
                return Err(error.with_fallback(json!({
                    "used": index > 0,
                    "exhausted": exhausted,
                    "attempts": attempts,
                    "note": if exhausted {
                        "every model in the chain was refused before it ran"
                    } else {
                        "stopped: this error is not a refusal before execution, so no further model was tried"
                    },
                })));
            }
        }
    }
    unreachable!("the chain always holds the first attempt")
}

#[cfg(test)]
mod tests {
    use super::{
        ApiError, MAX_FALLBACK_MODELS, attempts_in_order, refused_before_execution, validate,
    };
    use crate::platform::TaskInput;
    use serde_json::{Value, json};

    fn input(value: Value) -> TaskInput {
        serde_json::from_value(value).expect("task input")
    }

    #[test]
    fn only_refusals_before_execution_move_down_the_chain() {
        for code in [
            "resource_admission_unavailable",
            "context_window_exceeded",
            "embedding_item_exceeds_context",
            "model_exceeds_host_budget",
            "upstream_circuit_open",
            "model_not_installed",
            "model_not_eligible",
        ] {
            assert!(
                refused_before_execution(&ApiError::bad_request("x").with_code(code)),
                "{code}"
            );
        }
        // Started, cancelled, busy or uncoded errors never replay work on another model.
        for code in [
            "task_deadline_exceeded",
            "task_cancelled",
            "admission_timeout",
            "model_lifecycle_busy",
            "ollama_upstream_error",
        ] {
            assert!(
                !refused_before_execution(&ApiError::bad_request("x").with_code(code)),
                "{code}"
            );
        }
        assert!(!refused_before_execution(&ApiError::bad_request("x")));
        // A refusal after an earlier embedding slice ran is not a refusal before execution.
        assert!(!refused_before_execution(
            &ApiError::bad_request("x")
                .with_code("context_window_exceeded")
                .after_partial_execution()
        ));
    }

    #[test]
    fn validates_the_chain_before_any_model_runs() {
        let ok = input(json!({"prompt":"hi","model":"a:1","fallback_models":["b:1","c:1"]}));
        assert!(validate(&ok).is_ok());
        let too_long = input(json!({
            "prompt":"hi",
            "fallback_models": (0..=MAX_FALLBACK_MODELS).map(|n| format!("m{n}")).collect::<Vec<_>>(),
        }));
        assert!(validate(&too_long).is_err());
        let repeated = input(json!({"prompt":"hi","fallback_models":["b:1","b:1"]}));
        assert!(
            validate(&repeated)
                .unwrap_err()
                .message()
                .contains("repeats")
        );
        let blank = input(json!({"prompt":"hi","fallback_models":[" "]}));
        assert!(validate(&blank).is_err());
        let pinned = input(json!({
            "prompt":"hi","fallback_models":["b:1"],"expected_model_digest":"sha256:x",
        }));
        assert!(validate(&pinned).is_err());
    }

    #[test]
    fn an_entry_repeating_the_first_model_is_skipped() {
        // The first model may come from a scope default or a job's pick the caller never saw.
        let task = input(json!({"prompt":"hi","model":"a:1","fallback_models":["a:1","b:1"]}));
        assert!(validate(&task).is_ok());
        assert_eq!(
            attempts_in_order(&task),
            vec![Some("a:1".to_owned()), Some("b:1".to_owned())]
        );
        let routed = input(json!({"prompt":"hi","fallback_models":["b:1"]}));
        assert_eq!(
            attempts_in_order(&routed),
            vec![None, Some("b:1".to_owned())]
        );
    }
}
