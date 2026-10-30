//! Bounded admission for raw Ollama-compatible requests, with permits held for the response body.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::body::{Body, Bytes};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::{Notify, OwnedRwLockWriteGuard};

use crate::platform::resources::ResourcePermit;

/// Raw passthrough admission: a concurrency limit with a bounded wait, adjustable at runtime.
///
/// Raw callers cannot declare a task cost, so this is a plain count, not weighted admission.
/// Waiters are woken together and race for a freed slot: bounded, not strictly FIFO.
#[derive(Clone)]
pub struct RawAdmission {
    inner: Arc<RawAdmissionInner>,
}

struct RawAdmissionInner {
    state: std::sync::Mutex<RawAdmissionState>,
    changed: Notify,
}

#[derive(Debug)]
struct RawAdmissionState {
    active: usize,
    limit: usize,
    wait: Duration,
    waiting: usize,
    admitted: u64,
    rejected: u64,
    /// Exponentially weighted service time of finished raw requests, for `Retry-After`.
    average_ms: Option<u64>,
}

/// Why a raw request was refused, with a `Retry-After` estimate in seconds.
pub struct RawRejection {
    pub reason: &'static str,
    pub retry_after_seconds: u64,
}

impl RawAdmission {
    #[must_use]
    pub fn new(limit: usize, wait: Duration) -> Self {
        Self {
            inner: Arc::new(RawAdmissionInner {
                state: std::sync::Mutex::new(RawAdmissionState {
                    active: 0,
                    limit: limit.max(1),
                    wait,
                    waiting: 0,
                    admitted: 0,
                    rejected: 0,
                    average_ms: None,
                }),
                changed: Notify::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RawAdmissionState> {
        self.inner.state.lock().expect("raw admission poisoned")
    }

    /// Change the limit and wait; takes effect for the next request.
    pub fn set(&self, limit: usize, wait: Duration) {
        let mut state = self.lock();
        state.limit = limit.max(1);
        state.wait = wait;
        drop(state);
        self.inner.changed.notify_waiters();
    }

    #[must_use]
    pub fn wait(&self) -> Duration {
        self.lock().wait
    }

    #[must_use]
    pub fn receipt(&self) -> serde_json::Value {
        let state = self.lock();
        serde_json::json!({
            "limit": state.limit,
            "active": state.active,
            "waiting": state.waiting,
            "queue_wait_seconds": state.wait.as_secs(),
            "admitted": state.admitted,
            "rejected": state.rejected,
        })
    }

    fn retry_after(state: &RawAdmissionState) -> u64 {
        let per_request = state.average_ms.unwrap_or(5_000).max(250);
        let ahead = u64::try_from(state.waiting.saturating_add(1)).unwrap_or(u64::MAX);
        let lanes = u64::try_from(state.limit.max(1)).unwrap_or(1);
        (ahead.saturating_mul(per_request) / lanes)
            .div_ceil(1000)
            .clamp(1, 120)
    }

    /// Queue for a slot until `deadline`. The waiting count is bounded at eight per slot.
    pub(crate) async fn acquire(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<RawPermit, RawRejection> {
        {
            let mut state = self.lock();
            if state.active < state.limit {
                state.active += 1;
                state.admitted += 1;
                return Ok(RawPermit::new(self.clone()));
            }
            if tokio::time::Instant::now() >= deadline || state.waiting >= state.limit * 8 {
                state.rejected += 1;
                return Err(RawRejection {
                    reason: "raw request limit reached",
                    retry_after_seconds: Self::retry_after(&state),
                });
            }
            state.waiting += 1;
        }
        let result = loop {
            let changed = self.inner.changed.notified();
            {
                let mut state = self.lock();
                if state.active < state.limit {
                    state.active += 1;
                    state.admitted += 1;
                    break Ok(RawPermit::new(self.clone()));
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut state = self.lock();
                state.rejected += 1;
                break Err(RawRejection {
                    reason: "raw request limit reached",
                    retry_after_seconds: Self::retry_after(&state),
                });
            }
        };
        self.lock().waiting -= 1;
        result
    }
}

/// Held raw slot; released (and the service-time average updated) when the stream ends.
pub(crate) struct RawPermit {
    admission: RawAdmission,
    started: Instant,
}

impl RawPermit {
    fn new(admission: RawAdmission) -> Self {
        Self {
            admission,
            started: Instant::now(),
        }
    }
}

impl Drop for RawPermit {
    fn drop(&mut self) {
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut state = self.admission.lock();
        state.active = state.active.saturating_sub(1);
        state.average_ms = Some(state.average_ms.map_or(elapsed, |average| {
            average.saturating_mul(4).saturating_add(elapsed) / 5
        }));
        drop(state);
        self.admission.inner.changed.notify_waiters();
    }
}

/// Response body that owns the raw-admission permit for the complete upstream stream lifetime.
/// Receiving headers is not completion for Ollama: generation usually continues while bytes are
/// streamed, so releasing earlier turns a concurrency cap into a headers-only cap.
pub(crate) struct AdmittedBody {
    pub(crate) inner: Body,
    pub(crate) permit: Option<RawPermit>,
    pub(crate) execution: Option<OwnedRwLockWriteGuard<()>>,
    pub(crate) resource: Option<ResourcePermit>,
}

impl HttpBody for AdmittedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(context);
        if matches!(&result, Poll::Ready(None)) {
            self.permit.take();
            self.execution.take();
            self.resource.take();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
