//! Per-backend circuit breakers with a single half-open probe.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// Per-backend circuit breaker around upstream failures (transport errors, 500/502/504).
///
/// After `breaker_failures` consecutive failures the circuit opens for `breaker_cooldown_seconds`:
/// managed tasks fail fast with 503 and `Retry-After` instead of each queueing and then waiting
/// out a dead or crash-looping Ollama. After the cooldown one request is let through; its result
/// closes or re-opens the circuit.
#[derive(Clone, Default)]
pub(crate) struct Breakers {
    pub(crate) inner: Arc<StdMutex<HashMap<String, BreakerState>>>,
}

#[derive(Debug, Default)]
pub(crate) struct BreakerState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    probe_in_flight: bool,
    opened: u64,
}

/// The single request allowed through a half-open circuit. Dropping it without a recorded
/// outcome (the task failed before reaching Ollama) frees the probe slot for the next request.
pub(crate) struct BreakerProbe {
    pub(crate) breakers: Breakers,
    pub(crate) backend: String,
}

impl Drop for BreakerProbe {
    fn drop(&mut self) {
        if let Some(state) = self
            .breakers
            .inner
            .lock()
            .expect("breaker poisoned")
            .get_mut(&self.backend)
        {
            state.probe_in_flight = false;
        }
    }
}

impl Breakers {
    /// `Err(remaining)` while the circuit is open; `Ok(Some(probe))` for the one request allowed
    /// through after the cooldown.
    pub(crate) fn check(&self, backend: &str) -> Result<Option<BreakerProbe>, Duration> {
        let mut map = self.inner.lock().expect("breaker poisoned");
        let state = map.entry(backend.to_owned()).or_default();
        let Some(until) = state.open_until else {
            return Ok(None);
        };
        let now = Instant::now();
        if now < until {
            return Err(until - now);
        }
        if state.probe_in_flight {
            return Err(Duration::from_secs(1));
        }
        state.probe_in_flight = true;
        Ok(Some(BreakerProbe {
            breakers: self.clone(),
            backend: backend.to_owned(),
        }))
    }

    /// Returns true when this failure opened the circuit.
    pub(crate) fn record(
        &self,
        backend: &str,
        success: bool,
        threshold: u32,
        cooldown: Duration,
    ) -> bool {
        let mut map = self.inner.lock().expect("breaker poisoned");
        let state = map.entry(backend.to_owned()).or_default();
        let was_probe = state.open_until.is_some();
        if success {
            state.consecutive_failures = 0;
            state.open_until = None;
            return false;
        }
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if threshold > 0 && (was_probe || state.consecutive_failures >= threshold) {
            state.open_until = Some(Instant::now() + cooldown);
            state.opened += 1;
            return true;
        }
        false
    }

    pub(crate) fn receipt(&self) -> Value {
        let map = self.inner.lock().expect("breaker poisoned");
        let now = Instant::now();
        map.iter()
            .map(|(backend, state)| {
                (
                    backend.clone(),
                    json!({
                        "state": match state.open_until {
                            Some(until) if now < until => "open",
                            Some(_) => "half_open",
                            None => "closed",
                        },
                        "consecutive_failures": state.consecutive_failures,
                        "retry_after_seconds": state.open_until
                            .filter(|until| now < *until)
                            .map(|until| (until - now).as_secs().max(1)),
                        "times_opened": state.opened,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}
