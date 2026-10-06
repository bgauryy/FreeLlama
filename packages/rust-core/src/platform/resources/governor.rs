//! The cooperative admission governor: cached sampling, pressure holds and memory reservations.

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use super::pressure::{pressure_reasons, reason_observed, recovered_reason};
use super::{
    CapacityDenialReason, HostResources, LowMemoryMode, MemoryAdvisory, PressureReason,
    ResourceCapacityAssessment, ResourceDemand, ResourcePolicy, ResourceReceipt, ResourceSnapshot,
    ResourceWaitError, TelemetryPolicy, sample_host_resources,
};

/// Hold until the upstream response/stream has finished. All clones of a governor share this
/// accounting. Fresh OS telemetry can include already allocated reservations, so subtraction is
/// deliberately conservative; this is neither a physical allocation nor a guarantee of model fit.
#[derive(Debug)]
pub struct ResourcePermit {
    pub receipt: ResourceReceipt,
    reservations: Arc<AtomicU64>,
    changed: Arc<Notify>,
    bytes: u64,
}

impl ResourcePermit {
    /// Return the reservation early, once the load it covered is visible in OS telemetry.
    /// Keeping it until the response ended counted a loaded model twice (in OS available memory
    /// and in the reservation), so a second cold model needed about twice its real headroom.
    pub fn release(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        if bytes > 0 {
            self.reservations.fetch_sub(bytes, Ordering::AcqRel);
            // Wake capacity waiters now instead of at their next sampling tick.
            self.changed.notify_waiters();
        }
    }

    #[must_use]
    pub const fn reserved_bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for ResourcePermit {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Default)]
struct SampleState {
    saved: Option<(Instant, HostResources)>,
    pending: Option<JoinHandle<HostResources>>,
    sample_count: u64,
    holds: Vec<PressureHold>,
    stale: bool,
}

struct PressureHold {
    reason: PressureReason,
    healthy_samples: u32,
    since: Instant,
}

/// A hold whose signal stops being reported (a sensor that goes quiet after a throttle event)
/// could otherwise never recover, holding all local work until restart.
const MAX_UNOBSERVED_HOLD: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ResourceGovernor {
    policy: ResourcePolicy,
    sampler: Arc<dyn Fn() -> HostResources + Send + Sync>,
    state: Arc<Mutex<SampleState>>,
    pub(super) reservations: Arc<AtomicU64>,
    changed: Arc<Notify>,
}

/// Outcome of an atomic reservation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reservation {
    /// Reserved within the available-memory reserve.
    Fits,
    /// Reserved under [`LowMemoryMode::Advisory`] although it does not fit the reserve.
    BelowReserve,
    /// Nothing reserved: it does not fit and the policy is [`LowMemoryMode::Refuse`].
    Insufficient,
    /// Nothing reserved: the policy requires memory telemetry that is missing.
    TelemetryUnavailable,
    /// Nothing reserved: the accounting would overflow.
    Overflow,
}

impl std::fmt::Debug for ResourceGovernor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResourceGovernor")
            .field("policy", &self.policy)
            .field("reserved_bytes", &self.reservations.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Default for ResourceGovernor {
    fn default() -> Self {
        static DEFAULT: OnceLock<ResourceGovernor> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                Self::new(ResourcePolicy::default()).expect("default resource policy is valid")
            })
            .clone()
    }
}

impl ResourceGovernor {
    /// # Errors
    /// Returns an error for invalid hysteresis or sampling thresholds.
    pub fn new(policy: ResourcePolicy) -> Result<Self, String> {
        Self::with_sampler(policy, sample_host_resources)
    }

    /// The injected sampler must terminate in bounded time, like the built-in sampler. Sampling
    /// runs outside Tokio worker threads and is cached/singleflight across requests and clones.
    /// # Errors
    /// Returns an error for invalid hysteresis or sampling thresholds.
    pub fn with_sampler(
        policy: ResourcePolicy,
        sampler: impl Fn() -> HostResources + Send + Sync + 'static,
    ) -> Result<Self, String> {
        policy.validate()?;
        Ok(Self {
            policy,
            sampler: Arc::new(sampler),
            state: Arc::default(),
            reservations: Arc::new(AtomicU64::new(0)),
            changed: Arc::new(Notify::new()),
        })
    }

    /// Make the next [`Self::snapshot`] sample afresh, after `FreeLlama` itself changed what the
    /// host holds (for example by unloading idle models).
    pub async fn invalidate(&self) {
        self.state.lock().await.stale = true;
    }

    /// Read the latest cached observation, sampling once when stale.
    /// # Panics
    /// Panics only if the private sampler state violates its initialized-handle/cache invariant.
    pub async fn snapshot(&self) -> ResourceSnapshot {
        let mut state = self.state.lock().await;
        let needs_sample = state.stale
            || state
                .saved
                .as_ref()
                .is_none_or(|(at, _)| at.elapsed() >= self.policy.sample_interval);
        state.stale = false;
        if needs_sample {
            if state.pending.is_none() {
                let sampler = self.sampler.clone();
                state.pending = Some(tokio::task::spawn_blocking(move || sampler()));
            }
            // Keep the JoinHandle in shared state while awaiting: cancellation drops only the
            // lock guard, so the next reader resumes this sample instead of spawning another.
            let observed = state
                .pending
                .as_mut()
                .expect("sample started")
                .await
                .unwrap_or_else(|_| HostResources {
                    source: "sampler_failed".into(),
                    unavailable: vec!["sampler_failed".into()],
                    ..HostResources::default()
                });
            state.pending = None;
            let previous = state.saved.as_ref().map(|(_, sample)| sample);
            let triggered = pressure_reasons(&observed, previous, &self.policy);
            // Recover each signal independently. Low RAM must not preserve an obsolete CPU
            // hold forever and prevent safe reuse of an unchanged resident runner.
            let recovered: Vec<_> = state
                .holds
                .iter()
                .filter_map(|hold| {
                    let signal_expired = hold.since.elapsed() >= MAX_UNOBSERVED_HOLD;
                    (!triggered.contains(&hold.reason)
                        && (recovered_reason(hold.reason, &observed, previous, &self.policy)
                            || (signal_expired && !reason_observed(hold.reason, &observed))))
                    .then_some(hold.reason)
                })
                .collect();
            for reason in triggered {
                if !state.holds.iter().any(|hold| hold.reason == reason) {
                    state.holds.push(PressureHold {
                        reason,
                        healthy_samples: 0,
                        since: Instant::now(),
                    });
                }
            }
            state.holds.retain_mut(|hold| {
                hold.healthy_samples = if recovered.contains(&hold.reason) {
                    hold.healthy_samples.saturating_add(1)
                } else {
                    0
                };
                hold.healthy_samples < self.policy.recovery_samples
            });
            state.sample_count = state.sample_count.saturating_add(1);
            state.saved = Some((Instant::now(), observed));
        }
        let (saved_at, observation) = state.saved.as_ref().expect("sample is saved");
        let reserved_bytes = self.reservations.load(Ordering::Acquire);
        let holding = !state.holds.is_empty();
        ResourceSnapshot {
            status: if holding {
                "holding"
            } else if !observation
                .missing_required(self.policy.telemetry_policy)
                .is_empty()
            {
                "telemetry_unavailable"
            } else if observation.has_signal() {
                "ready"
            } else {
                "unknown"
            },
            holding,
            reasons: state.holds.iter().map(|hold| hold.reason).collect(),
            healthy_samples: state
                .holds
                .iter()
                .map(|hold| hold.healthy_samples)
                .min()
                .unwrap_or(0),
            sample_count: state.sample_count,
            sample_age_ms: saved_at.elapsed().as_millis(),
            reserved_bytes,
            effective_available_bytes: observation
                .available_memory_bytes
                .map(|bytes| bytes.saturating_sub(reserved_bytes)),
            hold_reserve_bytes: self.policy.reserve(observation.total_memory_bytes, false),
            resume_reserve_bytes: self.policy.reserve(observation.total_memory_bytes, true),
            observation: observation.clone(),
            policy: self.policy.clone(),
        }
    }

    /// Wait within the caller's existing admission deadline. A remote endpoint bypasses local
    /// telemetry. Required missing observations hold local work; best-effort admissions explicitly
    /// report unknown RAM. All policies retain previously observed pressure until recovery.
    /// # Errors
    /// Returns the last observed pressure snapshot if sampling or recovery exceeds the deadline.
    pub async fn wait_for_capacity(
        &self,
        upstream: &str,
        required_available_bytes: u64,
        timeout: Duration,
    ) -> Result<ResourcePermit, ResourceWaitError> {
        self.wait_for_demand(
            upstream,
            ResourceDemand::load(required_available_bytes),
            timeout,
        )
        .await
    }

    /// A permit holding `receipt.reserved_bytes` of this governor's shared accounting.
    fn permit(&self, receipt: ResourceReceipt) -> ResourcePermit {
        ResourcePermit {
            bytes: receipt.reserved_bytes,
            receipt,
            reservations: self.reservations.clone(),
            changed: self.changed.clone(),
        }
    }

    /// [`Self::wait_for_capacity`] for a [`ResourceDemand`]; see [`ResourceSnapshot::assess_demand`].
    /// # Errors
    /// Returns the last observed pressure snapshot if sampling or recovery exceeds the deadline.
    pub async fn wait_for_demand(
        &self,
        upstream: &str,
        demand: ResourceDemand,
        timeout: Duration,
    ) -> Result<ResourcePermit, ResourceWaitError> {
        let required_available_bytes = demand.bytes;
        let started = Instant::now();
        if !crate::net::is_loopback_url(upstream) {
            return Ok(self.permit(ResourceReceipt {
                status: "bypassed_remote",
                waited_ms: 0,
                required_available_bytes,
                reserved_bytes: 0,
                snapshot: None,
                assessment: None,
            }));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let first_sample_deadline =
            deadline.max(tokio::time::Instant::now() + self.policy.first_sample_grace);
        let mut last = None;
        let mut last_assessment = None;
        let mut recovering_capacity = false;
        loop {
            // Register before observing capacity, so a concurrent release cannot be missed.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let sample_deadline = if last.is_none() {
                first_sample_deadline
            } else {
                deadline
            };
            let Ok(snapshot) = tokio::time::timeout_at(sample_deadline, self.snapshot()).await
            else {
                break;
            };
            let mut assessment = snapshot.assess_demand(demand, recovering_capacity);
            let available = snapshot.observation.available_memory_bytes;
            let reserved = if !assessment.admissible {
                None
            } else if demand.resident && demand.bytes == 0 {
                Some(Reservation::Fits)
            } else {
                Some(self.try_reserve(
                    required_available_bytes,
                    available,
                    assessment.reserve_bytes,
                ))
            };
            match reserved {
                Some(outcome @ (Reservation::Fits | Reservation::BelowReserve)) => {
                    // Another admission may consume the headroom between the snapshot and
                    // reservation.
                    if outcome == Reservation::BelowReserve && assessment.memory_advisory.is_none()
                    {
                        assessment.memory_advisory = Some(MemoryAdvisory::BelowReserve);
                        assessment.status = "ready_low_memory_estimate";
                    }
                    let receipt = ResourceReceipt {
                        status: if assessment.memory_advisory.is_some() {
                            "admitted_low_memory_estimate"
                        } else if available.is_some() {
                            "admitted"
                        } else {
                            "admitted_telemetry_unknown"
                        },
                        waited_ms: started.elapsed().as_millis(),
                        required_available_bytes,
                        reserved_bytes: required_available_bytes,
                        snapshot: Some(snapshot),
                        assessment: Some(assessment),
                    };
                    return Ok(self.permit(receipt));
                }
                Some(refused) => deny_reservation(&mut assessment, refused),
                None => {}
            }
            recovering_capacity = true;
            last = Some(snapshot);
            last_assessment = Some(assessment);
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // A permit release wakes this waiter early; the sampling tick remains the fallback for
            // capacity freed outside FreeLlama.
            tokio::select! {
                () = changed => {},
                () = tokio::time::sleep_until(
                    (tokio::time::Instant::now() + self.policy.sample_interval).min(deadline),
                ) => {},
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        Err(ResourceWaitError {
            receipt: Box::new(ResourceReceipt {
                status: "deadline_exceeded",
                waited_ms: started.elapsed().as_millis(),
                required_available_bytes,
                reserved_bytes: 0,
                snapshot: last,
                assessment: last_assessment,
            }),
        })
    }

    /// Atomically reserve `bytes` against `available` minus `reserve`. Under
    /// [`LowMemoryMode::Advisory`] a shortfall still reserves and reports
    /// [`Reservation::BelowReserve`]; under [`LowMemoryMode::Refuse`] it reserves nothing.
    pub(super) fn try_reserve(
        &self,
        bytes: u64,
        available: Option<u64>,
        reserve: u64,
    ) -> Reservation {
        if available.is_none() && self.policy.telemetry_policy != TelemetryPolicy::BestEffort {
            return Reservation::TelemetryUnavailable;
        }
        let mut occupied = self.reservations.load(Ordering::Acquire);
        loop {
            let Some(next) = occupied.checked_add(bytes) else {
                return Reservation::Overflow;
            };
            let Some(required) = next.checked_add(reserve) else {
                return Reservation::Overflow;
            };
            let fits = available.is_none_or(|available| available >= required);
            if !fits && self.policy.low_memory == LowMemoryMode::Refuse {
                return Reservation::Insufficient;
            }
            match self.reservations.compare_exchange_weak(
                occupied,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if fits => return Reservation::Fits,
                Ok(_) => return Reservation::BelowReserve,
                Err(current) => occupied = current,
            }
        }
    }
}

/// Record why an admissible assessment still could not reserve: concurrent reservations changed
/// capacity between the snapshot and the atomic reservation, or its inputs were unusable.
fn deny_reservation(assessment: &mut ResourceCapacityAssessment, outcome: Reservation) {
    let (status, reason) = match outcome {
        Reservation::Fits | Reservation::BelowReserve => return,
        Reservation::Insufficient => (
            "held_insufficient_capacity",
            CapacityDenialReason::InsufficientCapacity,
        ),
        Reservation::TelemetryUnavailable => (
            "held_telemetry_unavailable",
            CapacityDenialReason::TelemetryUnavailable,
        ),
        Reservation::Overflow => (
            "held_arithmetic_overflow",
            CapacityDenialReason::ArithmeticOverflow,
        ),
    };
    assessment.admissible = false;
    assessment.status = status;
    assessment.denial_reason = Some(reason);
}
