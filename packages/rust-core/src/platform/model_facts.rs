//! What each model has shown it needs, learned from its own runs instead of assumed from its
//! name, and saved per model and digest in the shared database so a restart, or another
//! `FreeLlama` process, starts from the same evidence. A re-pulled model with a new digest starts
//! fresh: its behavior may have changed.
//!
//! - `output_need`: output tokens (reasoning included) one answer has needed. An answer cut off
//!   by a cap or by the end of its context (not a repetition loop) doubles it for next time.
//! - `runs:<task>` / `loops:<task>`: managed chat runs and the repetition loops among them, the
//!   evidence routing uses to stop preferring a model for work it keeps looping on.
//! - `next:<model>` (any digest): how often callers switched from this model to that one, which
//!   predictive preload reads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use super::{LoopRecord, TaskKind};
use crate::store::Store;

const OUTPUT_NEED: &str = "output_need";

type Key = (String, String, String);

#[derive(Clone, Default)]
pub(crate) struct ModelFacts {
    values: Arc<Mutex<BTreeMap<Key, u64>>>,
    store: Option<Store>,
}

fn key(model: &str, digest: Option<&str>, fact: &str) -> Key {
    (
        model.to_owned(),
        digest.unwrap_or_default().to_owned(),
        fact.to_owned(),
    )
}

impl ModelFacts {
    /// Start from what the database already knows.
    pub(crate) fn loaded(store: Option<Store>) -> Self {
        let values = store
            .as_ref()
            .and_then(|store| store.facts().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(model, digest, fact, value)| ((model, digest, fact), value))
            .collect();
        Self {
            values: Arc::new(Mutex::new(values)),
            store,
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<Key, u64>> {
        self.values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write off the async runtime; a failed write loses one sample, never the request.
    fn persist(&self, write: impl FnOnce(&Store) -> anyhow::Result<()> + Send + 'static) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let run = move || {
            static FAILURE_REPORTED: std::sync::Once = std::sync::Once::new();
            if let Err(error) = write(&store) {
                FAILURE_REPORTED.call_once(|| eprintln!("model facts not saved: {error:#}"));
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(run)),
            Err(_) => run(),
        }
    }

    /// Output tokens one answer from this model has needed, when observed.
    pub(crate) fn output_need(&self, model: &str, digest: Option<&str>) -> Option<u64> {
        self.lock().get(&key(model, digest, OUTPUT_NEED)).copied()
    }

    /// Learn from one finished chat response; `cap` is the caller's `num_predict`, if any, and
    /// `looped` marks a repetition loop, which says nothing about how much room was needed.
    pub(crate) fn learn_output(
        &self,
        model: &str,
        digest: Option<&str>,
        cap: Option<u64>,
        response: &Value,
        looped: bool,
    ) {
        if looped {
            return;
        }
        let Some(observed) = needed_output(cap, response) else {
            return;
        };
        let key = key(model, digest, OUTPUT_NEED);
        let updated = {
            let mut values = self.lock();
            let stored = values.get(&key).copied();
            let updated = next_need(stored, observed);
            if Some(updated) == stored {
                return;
            }
            values.insert(key.clone(), updated);
            updated
        };
        let (model, digest, fact) = key;
        self.persist(move |store| store.set_fact(&model, &digest, &fact, updated));
    }

    /// Count one managed chat run; `looped` when its output repeated itself.
    pub(crate) fn record_run(
        &self,
        model: &str,
        digest: Option<&str>,
        task: TaskKind,
        looped: bool,
    ) {
        let mut facts = vec![format!("runs:{}", task.as_str())];
        if looped {
            facts.push(format!("loops:{}", task.as_str()));
        }
        {
            let mut values = self.lock();
            for fact in &facts {
                *values.entry(key(model, digest, fact)).or_default() += 1;
            }
        }
        let (model, digest) = (model.to_owned(), digest.unwrap_or_default().to_owned());
        self.persist(move |store| {
            facts
                .iter()
                .try_for_each(|fact| store.add_fact(&model, &digest, fact, 1))
        });
    }

    /// Add one to a count that belongs to the model under any digest, such as which model a
    /// caller switched to after it.
    pub(crate) fn add_count(&self, model: &str, fact: &str) {
        *self.lock().entry(key(model, None, fact)).or_default() += 1;
        let (model, fact) = (model.to_owned(), fact.to_owned());
        self.persist(move |store| store.add_fact(&model, "", &fact, 1));
    }

    /// The digest-independent counts of `model` whose names start with `prefix`, keyed by the
    /// rest of the name.
    pub(crate) fn counts_with_prefix(&self, model: &str, prefix: &str) -> Vec<(String, u64)> {
        self.lock()
            .iter()
            .filter(|((name, digest, _), _)| name == model && digest.is_empty())
            .filter_map(|((_, _, fact), value)| {
                fact.strip_prefix(prefix)
                    .map(|rest| (rest.to_owned(), *value))
            })
            .collect()
    }

    /// Every task's run and loop counts for one model revision.
    pub(crate) fn loops_for(
        &self,
        model: &str,
        digest: Option<&str>,
    ) -> BTreeMap<TaskKind, LoopRecord> {
        let digest = digest.unwrap_or_default();
        let mut records: BTreeMap<TaskKind, LoopRecord> = BTreeMap::new();
        for ((name, revision, fact), value) in self.lock().iter() {
            if name != model || revision != digest {
                continue;
            }
            let Some((kind, task)) = fact.split_once(':') else {
                continue;
            };
            let Some(task) = TaskKind::from_wire(task) else {
                continue;
            };
            let count = u32::try_from(*value).unwrap_or(u32::MAX);
            let record = records.entry(task).or_default();
            match kind {
                "runs" => record.runs = count,
                "loops" => record.loops = count,
                _ => {}
            }
        }
        records
    }
}

/// What one response shows about the room the model needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    /// It finished on its own after this many output tokens.
    Finished(u64),
    /// A cap or the end of its context cut it off: it needed more than this.
    CutOff(u64),
}

fn needed_output(cap: Option<u64>, response: &Value) -> Option<Observed> {
    let produced = response["eval_count"].as_u64();
    match response["done_reason"].as_str()? {
        "stop" => produced.map(Observed::Finished),
        "length" => cap.or(produced).map(Observed::CutOff),
        _ => None,
    }
}

/// A cut-off answer doubles what the model gets; a finished one is always covered and otherwise
/// moves the estimate a quarter of the way toward it, so one unusually long answer does not
/// widen the context, and the memory it costs, for every request after it.
fn next_need(stored: Option<u64>, observed: Observed) -> u64 {
    match (stored, observed) {
        (stored, Observed::CutOff(tokens)) => stored.unwrap_or(0).max(tokens.saturating_mul(2)),
        (None, Observed::Finished(tokens)) => tokens,
        (Some(stored), Observed::Finished(tokens)) => {
            tokens.max(stored.saturating_mul(3).saturating_add(tokens) / 4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_model_that_ran_out_of_room_while_reasoning_gets_twice_the_room() {
        let facts = ModelFacts::default();
        let cut = json!({"done_reason":"length","eval_count":512,"message":{"content":"","thinking":"…"}});
        facts.learn_output("thinker", Some("d1"), Some(512), &cut, false);
        assert_eq!(facts.output_need("thinker", Some("d1")), Some(1024));
        // A shorter finished answer moves it a quarter of the way down, never below that answer.
        let done = json!({"done_reason":"stop","eval_count":700,"message":{"content":"ok"}});
        facts.learn_output("thinker", Some("d1"), Some(1024), &done, false);
        assert_eq!(facts.output_need("thinker", Some("d1")), Some(943));
        let long = json!({"done_reason":"stop","eval_count":1500,"message":{"content":"ok"}});
        facts.learn_output("thinker", Some("d1"), None, &long, false);
        assert_eq!(facts.output_need("thinker", Some("d1")), Some(1500));
        // Another revision of the same name starts fresh.
        assert_eq!(facts.output_need("thinker", Some("d2")), None);
        // An answer cut off mid-way needed more too; a repetition loop teaches nothing; with no
        // cap, the context ran out after what it produced.
        let capped =
            json!({"done_reason":"length","eval_count":512,"message":{"content":"partial"}});
        facts.learn_output("talker", None, Some(512), &capped, false);
        assert_eq!(facts.output_need("talker", None), Some(1024));
        facts.learn_output("looper", None, Some(512), &capped, true);
        assert_eq!(facts.output_need("looper", None), None);
        facts.learn_output("uncapped", None, None, &capped, false);
        assert_eq!(facts.output_need("uncapped", None), Some(1024));
    }

    #[test]
    fn facts_survive_a_restart_and_counts_from_two_processes_add_up() {
        let store = Store::in_memory().unwrap();
        let first = ModelFacts::loaded(Some(store.clone()));
        let second = ModelFacts::loaded(Some(store.clone()));
        first.record_run("m", Some("d"), TaskKind::Completion, true);
        second.record_run("m", Some("d"), TaskKind::Completion, false);
        first.learn_output(
            "m",
            Some("d"),
            Some(512),
            &json!({"done_reason":"stop","eval_count":900,"message":{"content":"x"}}),
            false,
        );
        let restarted = ModelFacts::loaded(Some(store));
        let record = restarted.loops_for("m", Some("d"))[&TaskKind::Completion];
        assert_eq!((record.runs, record.loops), (2, 1));
        assert_eq!(restarted.output_need("m", Some("d")), Some(900));
    }
}
