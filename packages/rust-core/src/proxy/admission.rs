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
use tokio::sync::{Notify, OwnedMutexGuard};

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
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let _waiting = Waiting(self);
        loop {
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
        }
    }
}

/// One queued caller in `waiting`, uncounted however the wait ends: with a slot, a rejection, or
/// a dropped request future when the client disconnects mid-wait.
struct Waiting<'a>(&'a RawAdmission);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.lock().waiting -= 1;
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

/// The backend load lock a raw generation holds while its model loads: until the runner is
/// observed ready, or the response ends. A request on a compatible resident runner holds none.
pub(crate) type LoadGuard = OwnedMutexGuard<()>;

pub(crate) type TransitionWatch = Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Response body that owns the raw-admission permit for the complete upstream stream lifetime.
/// Receiving headers is not completion for Ollama: generation usually continues while bytes are
/// streamed, so releasing earlier turns a concurrency cap into a headers-only cap.
pub(crate) struct AdmittedBody {
    pub(crate) inner: Body,
    pub(crate) permit: Option<RawPermit>,
    pub(crate) load: Option<LoadGuard>,
    pub(crate) resource: Option<ResourcePermit>,
    /// The platform's in-use mark for the model the request names.
    pub(crate) in_use: Option<Box<dyn Send>>,
    /// Reads what the response reports using and hands it on once the stream ends or the
    /// client goes away.
    pub(crate) usage: Option<UsageTap>,
    /// Polls alongside the response and releases the load lock once compatible residency is
    /// observed; the model's use lease (`in_use`) then keeps the runner from being evicted.
    pub(crate) transition_watch: Option<TransitionWatch>,
}

/// The scanner of one response and what to do with its counts.
pub(crate) struct UsageTap {
    pub(crate) scanner: super::usage::UsageScanner,
    pub(crate) requires_terminal: bool,
    pub(crate) finished: Option<Box<dyn FnOnce(super::usage::RawUsage, super::RawOutcome) + Send>>,
}

impl UsageTap {
    fn finish(&mut self, outcome: super::RawOutcome) {
        if let Some(finished) = self.finished.take() {
            finished(self.scanner.usage(), outcome);
        }
    }

    fn end_stream(&mut self) {
        self.scanner.end_stream();
        let outcome = if self.scanner.upstream_error() {
            super::RawOutcome::UpstreamError
        } else if self.requires_terminal && !self.scanner.terminal() {
            super::RawOutcome::Incomplete
        } else {
            super::RawOutcome::Complete
        };
        self.finish(outcome);
    }
}

impl Drop for UsageTap {
    fn drop(&mut self) {
        self.finish(super::RawOutcome::ClientDropped);
    }
}

impl HttpBody for AdmittedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(watch) = self.transition_watch.as_mut()
            && watch.as_mut().poll(context).is_ready()
        {
            self.transition_watch.take();
            self.load.take();
            // Host telemetry owns the allocation after compatible residency is observed.
            if crate::platform::host_has_unified_memory()
                && let Some(resource) = self.resource.as_mut()
            {
                resource.release();
            }
        }
        let result = Pin::new(&mut self.inner).poll_frame(context);
        if let (Poll::Ready(Some(Ok(frame))), Some(tap)) = (&result, self.usage.as_mut())
            && let Some(data) = frame.data_ref()
        {
            tap.scanner.feed(data);
        }
        if matches!(&result, Poll::Ready(None | Some(Err(_)))) {
            if let Some(mut tap) = self.usage.take() {
                if matches!(&result, Poll::Ready(Some(Err(_)))) {
                    tap.finish(super::RawOutcome::UpstreamError);
                } else {
                    tap.end_stream();
                }
            }
            self.transition_watch.take();
            self.permit.take();
            self.load.take();
            self.resource.take();
            self.in_use.take();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tap(
        outcomes: &Arc<std::sync::Mutex<Vec<super::super::RawOutcome>>>,
        requires_terminal: bool,
    ) -> UsageTap {
        let outcomes = Arc::clone(outcomes);
        UsageTap {
            scanner: super::super::usage::UsageScanner::default(),
            requires_terminal,
            finished: Some(Box::new(move |_, outcome| {
                outcomes.lock().unwrap().push(outcome);
            })),
        }
    }

    #[test]
    fn completion_error_incomplete_and_drop_have_distinct_outcomes() {
        use super::super::RawOutcome;
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (body, streaming, expected) in [
            (b"{\"done\":true}\n".as_slice(), true, RawOutcome::Complete),
            (
                b"{\"done\":false}\n".as_slice(),
                true,
                RawOutcome::Incomplete,
            ),
            (
                b"{\"error\":\"runner stopped\"}\n".as_slice(),
                true,
                RawOutcome::UpstreamError,
            ),
            (
                b"{\"embeddings\":[]}".as_slice(),
                false,
                RawOutcome::Complete,
            ),
        ] {
            let mut tap = tap(&outcomes, streaming);
            tap.scanner.feed(body);
            tap.end_stream();
            drop(tap);
            assert_eq!(outcomes.lock().unwrap().pop(), Some(expected));
            assert!(outcomes.lock().unwrap().is_empty());
        }
        drop(tap(&outcomes, true));
        assert_eq!(*outcomes.lock().unwrap(), [RawOutcome::ClientDropped]);
    }

    #[tokio::test]
    async fn verified_transition_releases_the_load_lock_but_keeps_the_use_lease_until_body_drop() {
        struct PendingBody;
        impl HttpBody for PendingBody {
            type Data = Bytes;
            type Error = std::convert::Infallible;
            fn poll_frame(
                self: Pin<&mut Self>,
                _: &mut TaskContext<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
                Poll::Pending
            }
        }
        struct Lease(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loading = Arc::clone(&lock).lock_owned().await;
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut body = AdmittedBody {
            inner: Body::new(PendingBody),
            permit: None,
            load: Some(loading),
            resource: None,
            in_use: Some(Box::new(Lease(Arc::clone(&released)))),
            usage: Some(tap(&outcomes, true)),
            transition_watch: Some(Box::pin(async {})),
        };
        // The watch owns no lease itself. Dropping a response before consumption releases
        // local ownership and reports disconnect, not completed upstream execution.
        assert!(lock.try_lock().is_err());
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            Pin::new(&mut body)
                .poll_frame(&mut TaskContext::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        // Readiness frees the next load immediately; the response still owns its use lease.
        assert!(lock.try_lock().is_ok());
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        drop(body);
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            *outcomes.lock().unwrap(),
            [super::super::RawOutcome::ClientDropped]
        );
    }

    #[tokio::test]
    async fn a_waiter_whose_client_disconnects_leaves_the_queue() {
        let admission = RawAdmission::new(1, Duration::from_secs(10));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let held = admission.acquire(deadline).await.ok().expect("first slot");
        let queued = {
            let admission = admission.clone();
            tokio::spawn(async move { admission.acquire(deadline).await.is_ok() })
        };
        while admission.receipt()["waiting"] != 1 {
            tokio::task::yield_now().await;
        }
        queued.abort();
        let _ = queued.await;
        assert_eq!(admission.receipt()["waiting"], 0);
        drop(held);
        assert!(admission.acquire(deadline).await.is_ok());
        assert_eq!(admission.receipt()["waiting"], 0);
    }
}
