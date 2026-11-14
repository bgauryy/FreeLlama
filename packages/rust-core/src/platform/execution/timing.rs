//! Per-phase timing of one managed task, reported in its receipt as `timing`.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Where a managed task's time went inside `serve`, from entering the pipeline to the finished
/// receipt, in milliseconds per phase. `upstream_ms` is Ollama's whole answer; every other phase
/// is `FreeLlama`'s own work, which the caller waits for.
pub(crate) struct PhaseClock {
    started: Instant,
    last: Instant,
    phases: serde_json::Map<String, Value>,
}

impl PhaseClock {
    pub(crate) fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            phases: serde_json::Map::new(),
        }
    }

    pub(crate) fn lap(&mut self, phase: &str) {
        let now = Instant::now();
        self.phases
            .insert(phase.to_owned(), json!(round_ms(now - self.last)));
        self.last = now;
    }

    pub(crate) fn receipt(&self) -> Value {
        let mut phases = self.phases.clone();
        let total = self.last - self.started;
        let upstream = phases
            .get("upstream_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        phases.insert("total_ms".into(), json!(round_ms(total)));
        phases.insert(
            "overhead_ms".into(),
            json!(((round_ms(total) - upstream) * 10.0).round() / 10.0),
        );
        Value::Object(phases)
    }
}

fn round_ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10_000.0).round() / 10.0
}
