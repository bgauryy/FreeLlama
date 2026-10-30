//! Observed repetition loops per model and task kind: the evidence routing uses, instead of a
//! model's name or architecture, to stop preferring a model for work it keeps looping on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{LoopRecord, TaskKind};

/// Process-local tallies of managed chat runs and the loops among them, keyed by model and task.
#[derive(Clone, Default)]
pub(crate) struct LoopEvidence(Arc<Mutex<BTreeMap<(String, TaskKind), LoopRecord>>>);

impl LoopEvidence {
    /// Count one completed managed run; `looped` when its output repeated itself.
    pub(crate) fn record(&self, model: &str, task: TaskKind, looped: bool) {
        let mut map = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = map.entry((model.to_owned(), task)).or_default();
        record.runs = record.runs.saturating_add(1);
        if looped {
            record.loops = record.loops.saturating_add(1);
        }
    }

    /// Every task's record for one model.
    pub(crate) fn for_model(&self, model: &str) -> BTreeMap<TaskKind, LoopRecord> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|((name, _), _)| name == model)
            .map(|((_, task), record)| (*task, *record))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_runs_and_loops_per_model_and_task() {
        let evidence = LoopEvidence::default();
        evidence.record("a", TaskKind::Completion, true);
        evidence.record("a", TaskKind::Completion, true);
        evidence.record("a", TaskKind::Completion, false);
        evidence.record("a", TaskKind::Vision, false);
        evidence.record("b", TaskKind::Completion, true);
        let a = evidence.for_model("a");
        assert_eq!(a[&TaskKind::Completion], LoopRecord { runs: 3, loops: 2 });
        assert!(a[&TaskKind::Completion].keeps_looping());
        assert!(!a[&TaskKind::Vision].keeps_looping());
        assert!(
            !evidence.for_model("b")[&TaskKind::Completion].keeps_looping(),
            "one loop is not a pattern"
        );
    }
}
