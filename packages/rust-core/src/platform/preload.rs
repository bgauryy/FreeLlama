//! Loading models before anyone asks for them: the operator's list at startup, and the model that
//! usually comes next after the one just used.
//!
//! A cold load costs seconds to tens of seconds; a resident model answers at once. Startup
//! preload fills that gap for the models an operator knows will be used. Predictive preload
//! learns it: every switch from one model to another within `follow_window_seconds` is counted
//! in the shared database, and once a model's successor is clear (`min_probability` of at least
//! `min_observations` switches), finishing work on it loads that successor in the background.
//! A guess never costs anyone else: a predicted load only runs when it fits without unloading
//! anything, the host is under no pressure, and nothing else is waiting for the backend.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Backend, PlatformState, TaskPriority, audit, catalog, execution, warming};

const NEXT_PREFIX: &str = "next:";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PreloadPolicy {
    /// Models to load when the server starts, in order. Each is a tag or a table with
    /// `model`, and optionally `context_tokens`, `keep_alive` and `messages`, a prompt prefix
    /// (such as a long system prompt) evaluated into the runner's prompt cache.
    pub startup: Vec<PreloadModel>,
    /// Load the model that usually follows the one just used.
    pub predictive: bool,
    /// Share of observed switches away from a model that must go to one successor.
    pub min_probability: f64,
    /// Switches away from a model needed before its successor is predicted.
    pub min_observations: u64,
    /// A request for another model this soon after one counts as a switch between them.
    pub follow_window_seconds: u64,
}

impl Default for PreloadPolicy {
    fn default() -> Self {
        Self {
            startup: Vec::new(),
            predictive: true,
            min_probability: 0.5,
            min_observations: 3,
            follow_window_seconds: 600,
        }
    }
}

impl PreloadPolicy {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.min_probability.is_finite()
                && self.min_probability > 0.0
                && self.min_probability <= 1.0
                && self.min_observations > 0
                && self.follow_window_seconds > 0,
            "preload requires min_probability in (0,1], and positive min_observations and follow_window_seconds"
        );
        anyhow::ensure!(
            self.startup
                .iter()
                .all(|entry| !entry.model().trim().is_empty()),
            "every preload.startup entry needs a model tag"
        );
        Ok(())
    }
}

/// One startup preload: a tag, or a tag with how to load it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum PreloadModel {
    Tag(String),
    Detailed(PreloadSpec),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PreloadSpec {
    pub model: String,
    pub context_tokens: Option<u64>,
    /// Defaults to the warming policy's `max_seconds`; pin the model to keep it past that.
    pub keep_alive: Option<String>,
    /// Chat messages evaluated into the runner's prompt cache, so requests that start with the
    /// same messages skip evaluating them again.
    pub messages: Option<Vec<Value>>,
}

impl PreloadModel {
    fn model(&self) -> &str {
        match self {
            Self::Tag(model) => model,
            Self::Detailed(spec) => &spec.model,
        }
    }

    fn spec(&self) -> PreloadSpec {
        match self {
            Self::Tag(model) => PreloadSpec {
                model: model.clone(),
                context_tokens: None,
                keep_alive: None,
                messages: None,
            },
            Self::Detailed(spec) => spec.clone(),
        }
    }
}

/// What this process remembers between requests.
#[derive(Clone, Default)]
pub(crate) struct Preloader {
    /// The model that served last, and when it finished.
    last: Arc<Mutex<Option<(String, Instant)>>>,
    /// One predicted load at a time.
    predicting: Arc<AtomicBool>,
    /// Startup entries already loaded, so a reload of the runtime file loads only new ones.
    started: Arc<Mutex<Vec<PreloadModel>>>,
}

/// The successor of `model` when the switches seen so far make it clear.
pub(super) fn predicted_next(
    facts: &[(String, u64)],
    min_probability: f64,
    min_observations: u64,
) -> Option<String> {
    let total = facts.iter().map(|(_, count)| *count).sum::<u64>();
    if total < min_observations {
        return None;
    }
    let (next, count) = facts
        .iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))?;
    (super::float(*count) / super::float(total) >= min_probability).then(|| next.clone())
}

/// Note that `model` on `backend` finished a request a caller made, learn the switch from the
/// model before it, and start a predicted load of its usual successor.
pub(super) fn served(state: &PlatformState, backend: &str, model: &str) {
    let tunables = state.tunables();
    let policy = &tunables.preload;
    let window = Duration::from_secs(policy.follow_window_seconds);
    let previous = {
        let mut last = state
            .preload
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.replace((model.to_owned(), Instant::now()))
    };
    if let Some((previous, at)) = previous
        && previous != model
        && at.elapsed() <= window
    {
        state
            .model_facts
            .add_count(&previous, &format!("{NEXT_PREFIX}{model}"));
    }
    if !policy.predictive || backend != Backend::Gpu.as_str() {
        return;
    }
    let successors = state.model_facts.counts_with_prefix(model, NEXT_PREFIX);
    let Some(next) = predicted_next(&successors, policy.min_probability, policy.min_observations)
    else {
        return;
    };
    if state.preload.predicting.swap(true, Ordering::AcqRel) {
        return;
    }
    let state = state.clone();
    let after = model.to_owned();
    tokio::spawn(async move {
        let outcome = predicted_load(&state, &next).await;
        state.preload.predicting.store(false, Ordering::Release);
        if let Some(outcome) = outcome {
            record(&state, &next, "predicted", &after, outcome);
        }
    });
}

/// Load `model` when that costs nobody anything; `None` when it was skipped.
async fn predicted_load(state: &PlatformState, model: &str) -> Option<Result<u64, String>> {
    if state.cpu_models.contains(model) {
        return None;
    }
    let target = execution::primary_target(state);
    let receipt = target.admission.receipt();
    if receipt["in_flight"] != 0 || receipt["queue_depth"].as_u64().unwrap_or(0) > 0 {
        return None;
    }
    let catalog_model = catalog::primary_model(state, model).await?;
    if catalog_model.resident
        || super::raw_models::may_need_room(state, &target, &catalog_model).await
    {
        return None;
    }
    Some(
        warm(
            state,
            PreloadSpec {
                model: model.to_owned(),
                context_tokens: None,
                keep_alive: None,
                messages: None,
            },
            false,
        )
        .await,
    )
}

/// Load the startup list once the server is up, then again for entries a reloaded runtime file
/// adds. Each runs as a background warm, so admission, the memory budget and host pressure
/// apply as they do to any other load.
pub(super) fn start(state: &PlatformState) {
    let startup = state.tunables().preload.startup.clone();
    let (pending, removed): (Vec<PreloadModel>, Vec<String>) = {
        let mut started = state
            .preload
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A model whose entry left the file was loaded for that entry alone; it is released
        // below unless another entry still names it.
        let removed = started
            .iter()
            .filter(|entry| !startup.contains(entry))
            .map(|entry| entry.model().to_owned())
            .filter(|model| !startup.iter().any(|entry| entry.model() == model))
            .collect::<Vec<_>>();
        started.retain(|entry| startup.contains(entry));
        let pending = startup
            .into_iter()
            .filter(|entry| !started.contains(entry))
            .collect::<Vec<_>>();
        started.extend(pending.iter().cloned());
        (pending, removed)
    };
    if pending.is_empty() && removed.is_empty() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        for model in removed {
            // Only an idle runner is released; one serving work keeps its normal expiry.
            if state.activity.usage(Backend::Gpu.as_str(), &model).active > 0 {
                continue;
            }
            match execution::unload_resident_model(&state, &model, Duration::from_secs(1)).await {
                Ok(Some(receipt)) if receipt["unloaded"] == true => {
                    eprintln!("preload entry removed; unloaded {model}");
                }
                Ok(_) | Err(_) => {}
            }
        }
        for entry in pending {
            let model = entry.model().to_owned();
            let outcome = warm(&state, entry.spec(), true).await;
            match &outcome {
                Ok(ms) => eprintln!("preloaded {model} in {ms} ms"),
                Err(error) => eprintln!("preload of {model} failed: {error}"),
            }
            record(&state, &model, "startup", "", outcome);
        }
    });
}

async fn warm(state: &PlatformState, spec: PreloadSpec, startup: bool) -> Result<u64, String> {
    let started = Instant::now();
    let keep_alive = spec
        .keep_alive
        .or_else(|| startup.then(|| format!("{}s", state.tunables().warming.max_seconds)));
    let input = warming::preload_task(
        spec.model,
        spec.context_tokens,
        keep_alive,
        spec.messages.unwrap_or_default(),
        TaskPriority::Background,
    )
    .map_err(|error| error.body.error)?;
    execution::execute_task(State(state.clone()), Json(input))
        .await
        .map(|_| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
        .map_err(|error| error.body.error)
}

fn record(
    state: &PlatformState,
    model: &str,
    trigger: &str,
    after: &str,
    outcome: Result<u64, String>,
) {
    let trigger = if after.is_empty() {
        trigger.to_owned()
    } else {
        format!("{trigger}_after:{after}")
    };
    state.audit.record(&audit::Entry {
        backend: Backend::Gpu.as_str(),
        model,
        event: if outcome.is_ok() {
            "preload"
        } else {
            "preload_failed"
        },
        trigger: &trigger,
        duration_ms: outcome.ok(),
        bytes: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs
            .iter()
            .map(|(name, count)| ((*name).to_owned(), *count))
            .collect()
    }

    #[test]
    fn a_successor_is_predicted_only_once_it_is_clear() {
        assert_eq!(predicted_next(&counts(&[("b", 2)]), 0.5, 3), None);
        assert_eq!(
            predicted_next(&counts(&[("b", 3)]), 0.5, 3).as_deref(),
            Some("b")
        );
        assert_eq!(
            predicted_next(&counts(&[("b", 2), ("c", 2), ("d", 1)]), 0.5, 3),
            None
        );
        assert_eq!(
            predicted_next(&counts(&[("b", 6), ("c", 2)]), 0.5, 3).as_deref(),
            Some("b")
        );
        // A tie goes to the name that sorts first, so the choice is stable.
        assert_eq!(
            predicted_next(&counts(&[("c", 3), ("b", 3)]), 0.5, 3).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn startup_entries_parse_as_tags_or_tables() {
        let policy: PreloadPolicy = toml::from_str(
            "startup = [\"a:1b\", { model = \"b:2b\", keep_alive = \"30m\", messages = [{ role = \"system\", content = \"You review code.\" }] }]\npredictive = false\n",
        )
        .unwrap();
        assert_eq!(policy.startup[0], PreloadModel::Tag("a:1b".into()));
        let PreloadModel::Detailed(spec) = &policy.startup[1] else {
            panic!("table entry");
        };
        assert_eq!(spec.keep_alive.as_deref(), Some("30m"));
        assert_eq!(spec.messages.as_ref().unwrap()[0]["role"], "system");
        assert!(!policy.predictive);
        assert!(policy.validate().is_ok());
        let bad = PreloadPolicy {
            min_probability: 0.0,
            ..PreloadPolicy::default()
        };
        assert!(bad.validate().is_err());
    }
}
