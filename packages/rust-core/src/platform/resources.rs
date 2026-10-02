//! Cached host telemetry and a cooperative admission governor. It never changes OS/Ollama
//! settings or interrupts inference; a permit only accounts for additional expected memory.

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::{collections::BTreeMap, io::Read};

use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostResources {
    pub source: String,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub available_memory_kind: String,
    pub free_memory_bytes: Option<u64>,
    pub reclaimable_memory_bytes: Option<u64>,
    pub occupied_memory_bytes: Option<u64>,
    pub compressor_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    /// Cumulative pages; only growth between samples is a pressure signal.
    pub swap_out_pages: Option<u64>,
    pub memory_pressure: Option<MemoryPressure>,
    pub load_average_one_minute: Option<f64>,
    pub logical_cpus: Option<u32>,
    /// `None` means the OS did not provide a thermal observation, including pmset's
    /// "No CPU power status has been recorded" response.
    pub thermal_throttled: Option<bool>,
    pub cpu_speed_limit_percent: Option<u32>,
    pub thermal_warning_level: Option<u32>,
    /// cgroup v2 `memory.max` of this process when it is below physical RAM (containers).
    pub cgroup_memory_limit_bytes: Option<u64>,
    /// Linux PSI `some avg10` for memory: % of the last 10s some task stalled on memory.
    pub memory_psi_some_avg10: Option<f64>,
    /// Discrete-GPU memory summed over devices (NVIDIA via nvidia-smi, AMD via sysfs). `None` on
    /// unified-memory hosts and when no GPU tool is readable.
    pub gpu_memory_total_bytes: Option<u64>,
    pub gpu_memory_free_bytes: Option<u64>,
    pub gpu_telemetry_source: Option<String>,
    pub unavailable: Vec<String>,
}

/// Missing observations never clear known pressure. This policy controls only whether an
/// otherwise unheld local request may proceed with incomplete telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryPolicy {
    BestEffort,
    RequireMemory,
    /// Require RAM and CPU load, plus OS pressure and thermal readings where supported.
    RequireAll,
}

impl Default for TelemetryPolicy {
    /// Require RAM telemetry where a collector exists. Elsewhere (Windows) a memory requirement
    /// could never be met and every local request was refused, so admission there is best-effort
    /// and says so in each receipt (`admitted_telemetry_unknown`).
    fn default() -> Self {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            Self::RequireMemory
        } else {
            Self::BestEffort
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryMetric {
    AvailableMemory,
    CpuLoad,
    OsMemoryPressure,
    ThermalPressure,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourcePolicy {
    pub telemetry_policy: TelemetryPolicy,
    pub sample_interval: Duration,
    pub hold_available_percent: u32,
    pub resume_available_percent: u32,
    pub hold_available_min_bytes: u64,
    pub resume_available_min_bytes: u64,
    pub hold_load_per_cpu: f64,
    pub resume_load_per_cpu: f64,
    pub recovery_samples: u32,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            telemetry_policy: TelemetryPolicy::default(),
            sample_interval: Duration::from_secs(2),
            hold_available_percent: 15,
            resume_available_percent: 20,
            hold_available_min_bytes: GIB,
            resume_available_min_bytes: 2 * GIB,
            hold_load_per_cpu: 1.5,
            resume_load_per_cpu: 1.0,
            recovery_samples: 2,
        }
    }
}

impl ResourcePolicy {
    fn validate(&self) -> Result<(), String> {
        if self.sample_interval.is_zero()
            || self.hold_available_percent >= self.resume_available_percent
            || self.resume_available_percent > 100
            || self.hold_available_min_bytes >= self.resume_available_min_bytes
            || !self.hold_load_per_cpu.is_finite()
            || !self.resume_load_per_cpu.is_finite()
            || self.resume_load_per_cpu <= 0.0
            || self.hold_load_per_cpu <= self.resume_load_per_cpu
            || self.recovery_samples == 0
        {
            return Err("resource policy requires positive sampling/recovery, increasing memory reserves and decreasing load thresholds".into());
        }
        Ok(())
    }

    fn reserve(&self, total: Option<u64>, recovering: bool) -> u64 {
        let (percent, minimum) = if recovering {
            (
                self.resume_available_percent,
                self.resume_available_min_bytes,
            )
        } else {
            (self.hold_available_percent, self.hold_available_min_bytes)
        };
        total.map_or(minimum, |bytes| {
            (bytes / 100 * u64::from(percent)).max(minimum)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureReason {
    LowAvailableMemory,
    OsMemoryPressure,
    HighCpuLoad,
    ThermalThrottling,
    ActiveSwapping,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceSnapshot {
    pub status: &'static str,
    pub holding: bool,
    pub reasons: Vec<PressureReason>,
    pub healthy_samples: u32,
    pub sample_count: u64,
    pub sample_age_ms: u128,
    pub reserved_bytes: u64,
    pub effective_available_bytes: Option<u64>,
    pub hold_reserve_bytes: u64,
    pub resume_reserve_bytes: u64,
    pub observation: HostResources,
    pub policy: ResourcePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityDenialReason {
    HostPressure,
    TelemetryUnavailable,
    InsufficientCapacity,
    ArithmeticOverflow,
}

/// Point-in-time capacity advice, without acquiring a permit or changing reservations.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceCapacityAssessment {
    pub status: &'static str,
    pub admissible: bool,
    pub denial_reason: Option<CapacityDenialReason>,
    pub required_available_bytes: u64,
    pub reserve_bytes: u64,
    pub required_with_reserve_bytes: Option<u64>,
    pub effective_available_bytes: Option<u64>,
    pub missing_telemetry: Vec<TelemetryMetric>,
}

impl ResourceSnapshot {
    /// Evaluate this observation using the same policy as execution. Recovery uses the higher
    /// reserve after a failed admission attempt. This does not guarantee future dispatch capacity.
    #[must_use]
    pub fn assess_capacity(
        &self,
        required_available_bytes: u64,
        recovering_capacity: bool,
    ) -> ResourceCapacityAssessment {
        self.assess_demand(
            ResourceDemand::load(required_available_bytes),
            recovering_capacity,
        )
    }

    /// Like [`Self::assess_capacity`], but a request to an already-resident runner adds no memory
    /// and is not held by low available memory alone: on unified memory the resident model is
    /// usually what lowered it, and holding its own requests locked the model out until
    /// `keep_alive` unloaded it. OS pressure, swapping, CPU load and thermal holds still apply.
    #[must_use]
    pub fn assess_demand(
        &self,
        demand: ResourceDemand,
        recovering_capacity: bool,
    ) -> ResourceCapacityAssessment {
        let required_available_bytes = demand.bytes;
        let resident_only = demand.resident && demand.bytes == 0;
        let low_memory_only = self
            .reasons
            .iter()
            .all(|reason| *reason == PressureReason::LowAvailableMemory);
        let reserve_bytes = if recovering_capacity {
            self.resume_reserve_bytes
        } else {
            self.hold_reserve_bytes
        };
        let required_with_reserve_bytes = required_available_bytes.checked_add(reserve_bytes);
        let missing_telemetry = self
            .observation
            .missing_required(self.policy.telemetry_policy);
        let denial_reason = if self.holding && !(resident_only && low_memory_only) {
            Some(CapacityDenialReason::HostPressure)
        } else if !missing_telemetry.is_empty() {
            Some(CapacityDenialReason::TelemetryUnavailable)
        } else if required_with_reserve_bytes.is_none()
            || self
                .reserved_bytes
                .checked_add(required_available_bytes)
                .and_then(|bytes| bytes.checked_add(reserve_bytes))
                .is_none()
        {
            Some(CapacityDenialReason::ArithmeticOverflow)
        } else if !resident_only
            && self.effective_available_bytes.is_some_and(|available| {
                required_with_reserve_bytes.is_some_and(|required| available < required)
            })
        {
            Some(CapacityDenialReason::InsufficientCapacity)
        } else {
            None
        };
        let status = match denial_reason {
            Some(CapacityDenialReason::HostPressure) => "held_host_pressure",
            Some(CapacityDenialReason::TelemetryUnavailable) => "held_telemetry_unavailable",
            Some(CapacityDenialReason::InsufficientCapacity) => "held_insufficient_capacity",
            Some(CapacityDenialReason::ArithmeticOverflow) => "held_arithmetic_overflow",
            None if self.observation.available_memory_bytes.is_none() => "ready_telemetry_unknown",
            None => "ready",
        };
        ResourceCapacityAssessment {
            status,
            admissible: denial_reason.is_none(),
            denial_reason,
            required_available_bytes,
            reserve_bytes,
            required_with_reserve_bytes,
            effective_available_bytes: self.effective_available_bytes,
            missing_telemetry,
        }
    }
}

/// What a request adds to host memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResourceDemand {
    pub bytes: u64,
    /// The runner is already loaded with a compatible configuration.
    pub resident: bool,
}

impl ResourceDemand {
    #[must_use]
    pub const fn load(bytes: u64) -> Self {
        Self {
            bytes,
            resident: false,
        }
    }

    #[must_use]
    pub const fn resident() -> Self {
        Self {
            bytes: 0,
            resident: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceReceipt {
    pub status: &'static str,
    pub waited_ms: u128,
    pub required_available_bytes: u64,
    pub reserved_bytes: u64,
    pub snapshot: Option<ResourceSnapshot>,
    pub assessment: Option<ResourceCapacityAssessment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceWaitError {
    pub receipt: Box<ResourceReceipt>,
}

impl std::fmt::Display for ResourceWaitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "host resource admission deadline exceeded; retry when telemetry and capacity permit",
        )
    }
}
impl std::error::Error for ResourceWaitError {}

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
    reservations: Arc<AtomicU64>,
    changed: Arc<Notify>,
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
            changed: Arc::default(),
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
        if !is_loopback(upstream) {
            return Ok(ResourcePermit {
                receipt: ResourceReceipt {
                    status: "bypassed_remote",
                    waited_ms: 0,
                    required_available_bytes,
                    reserved_bytes: 0,
                    snapshot: None,
                    assessment: None,
                },
                reservations: self.reservations.clone(),
                changed: self.changed.clone(),
                bytes: 0,
            });
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = None;
        let mut last_assessment = None;
        let mut recovering_capacity = false;
        loop {
            // Register before observing capacity, so a concurrent release cannot be missed.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let Ok(snapshot) = tokio::time::timeout_at(deadline, self.snapshot()).await else {
                break;
            };
            let mut assessment = snapshot.assess_demand(demand, recovering_capacity);
            let available = snapshot.observation.available_memory_bytes;
            if assessment.admissible
                && (demand.resident && demand.bytes == 0
                    || self.try_reserve(
                        required_available_bytes,
                        available,
                        assessment.reserve_bytes,
                    ))
            {
                let receipt = ResourceReceipt {
                    status: if available.is_some() && snapshot.status == "ready" {
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
                return Ok(ResourcePermit {
                    receipt,
                    reservations: self.reservations.clone(),
                    changed: self.changed.clone(),
                    bytes: required_available_bytes,
                });
            }
            // Another admission may consume capacity between the snapshot and atomic reservation.
            if assessment.admissible {
                assessment.admissible = false;
                assessment.status = "held_insufficient_capacity";
                assessment.denial_reason = Some(CapacityDenialReason::InsufficientCapacity);
            }
            recovering_capacity = true;
            last = Some(snapshot);
            last_assessment = Some(assessment);
            if tokio::time::Instant::now() >= deadline {
                break;
            }
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

    fn try_reserve(&self, bytes: u64, available: Option<u64>, reserve: u64) -> bool {
        if available.is_none() && self.policy.telemetry_policy != TelemetryPolicy::BestEffort {
            return false;
        }
        self.reservations
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |occupied| {
                let next = occupied.checked_add(bytes)?;
                let required = next.checked_add(reserve)?;
                if available.is_some_and(|available| available < required) {
                    None
                } else {
                    Some(next)
                }
            })
            .is_ok()
    }
}

impl HostResources {
    fn missing_required(&self, policy: TelemetryPolicy) -> Vec<TelemetryMetric> {
        let mut missing = Vec::new();
        if policy == TelemetryPolicy::BestEffort {
            return missing;
        }
        if self.available_memory_bytes.is_none() {
            missing.push(TelemetryMetric::AvailableMemory);
        }
        if policy == TelemetryPolicy::RequireAll {
            if self.load_per_cpu().is_none() {
                missing.push(TelemetryMetric::CpuLoad);
            }
            // The built-in Linux collector explicitly lacks these two interfaces. For unknown
            // collectors, require them rather than guessing that missing means unsupported.
            if self.source != "linux_proc" {
                if self.memory_pressure.is_none() {
                    missing.push(TelemetryMetric::OsMemoryPressure);
                }
                if self.thermal_throttled.is_none() {
                    missing.push(TelemetryMetric::ThermalPressure);
                }
            }
        }
        missing
    }

    fn load_per_cpu(&self) -> Option<f64> {
        let load = self
            .load_average_one_minute
            .filter(|load| load.is_finite() && *load >= 0.0)?;
        let cores = self.logical_cpus.filter(|cores| *cores > 0)?;
        Some(load / f64::from(cores))
    }

    fn has_signal(&self) -> bool {
        self.available_memory_bytes.is_some()
            || self.load_per_cpu().is_some()
            || self.memory_pressure.is_some()
            || self.thermal_throttled.is_some()
    }
}

fn swap_grew(current: &HostResources, previous: Option<&HostResources>) -> Option<bool> {
    current
        .swap_out_pages
        .zip(previous?.swap_out_pages)
        .map(|(now, before)| now > before)
}

fn pressure_reasons(
    current: &HostResources,
    previous: Option<&HostResources>,
    policy: &ResourcePolicy,
) -> Vec<PressureReason> {
    let mut reasons = Vec::new();
    if current
        .available_memory_bytes
        .is_some_and(|available| available < policy.reserve(current.total_memory_bytes, false))
    {
        reasons.push(PressureReason::LowAvailableMemory);
    }
    if matches!(
        current.memory_pressure,
        Some(MemoryPressure::Warning | MemoryPressure::Critical)
    ) {
        reasons.push(PressureReason::OsMemoryPressure);
    }
    if current
        .load_per_cpu()
        .is_some_and(|load| load >= policy.hold_load_per_cpu)
    {
        reasons.push(PressureReason::HighCpuLoad);
    }
    if current.thermal_throttled == Some(true) {
        reasons.push(PressureReason::ThermalThrottling);
    }
    // Historical swap occupancy persists long after pressure ends. Only fresh paging while below
    // the recovery reserve holds work; swapping alone is not evidence of present saturation.
    if swap_grew(current, previous) == Some(true)
        && current
            .available_memory_bytes
            .is_some_and(|available| available < policy.reserve(current.total_memory_bytes, true))
    {
        reasons.push(PressureReason::ActiveSwapping);
    }
    reasons
}

fn recovered_reason(
    reason: PressureReason,
    current: &HostResources,
    previous: Option<&HostResources>,
    policy: &ResourcePolicy,
) -> bool {
    match reason {
        PressureReason::LowAvailableMemory => current
            .available_memory_bytes
            .is_some_and(|available| available >= policy.reserve(current.total_memory_bytes, true)),
        PressureReason::OsMemoryPressure => current.memory_pressure == Some(MemoryPressure::Normal),
        PressureReason::HighCpuLoad => current
            .load_per_cpu()
            .is_some_and(|load| load <= policy.resume_load_per_cpu),
        PressureReason::ThermalThrottling => current.thermal_throttled == Some(false),
        PressureReason::ActiveSwapping => {
            swap_grew(current, previous) == Some(false)
                && current.available_memory_bytes.is_some_and(|available| {
                    available >= policy.reserve(current.total_memory_bytes, true)
                })
        }
    }
}

/// Whether the current sample carries the signal a pressure reason is judged on.
fn reason_observed(reason: PressureReason, current: &HostResources) -> bool {
    match reason {
        PressureReason::LowAvailableMemory | PressureReason::ActiveSwapping => {
            current.available_memory_bytes.is_some()
        }
        PressureReason::OsMemoryPressure => current.memory_pressure.is_some(),
        PressureReason::HighCpuLoad => current.load_per_cpu().is_some(),
        PressureReason::ThermalThrottling => current.thermal_throttled.is_some(),
    }
}

fn is_loopback(upstream: &str) -> bool {
    reqwest::Url::parse(upstream)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

/// Collect one bounded, unprivileged observation; unsupported fields stay `None`.
#[must_use]
pub fn sample_host_resources() -> HostResources {
    #[cfg(target_os = "macos")]
    {
        sample_macos()
    }
    #[cfg(target_os = "linux")]
    {
        sample_linux()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        HostResources {
            source: "unsupported_platform".into(),
            unavailable: vec!["host_resource_telemetry".into()],
            ..HostResources::default()
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux(meminfo: &str, loadavg: &str, vmstat: &str, cpus: Option<u32>) -> HostResources {
    let values = colon_numbers(meminfo);
    let bytes = |name| values.get(name).and_then(|value| value.checked_mul(1024));
    let total = bytes("MemTotal");
    let available = bytes("MemAvailable").filter(|value| total.is_none_or(|total| *value <= total));
    let free = bytes("MemFree");
    let swap_used = bytes("SwapTotal")
        .zip(bytes("SwapFree"))
        .and_then(|(total, free)| total.checked_sub(free));
    HostResources {
        source: "linux_proc".into(),
        total_memory_bytes: total,
        available_memory_bytes: available,
        available_memory_kind: "kernel_mem_available_estimate".into(),
        free_memory_bytes: free,
        reclaimable_memory_bytes: available
            .zip(free)
            .map(|(available, free)| available.saturating_sub(free)),
        occupied_memory_bytes: total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available)),
        compressor_bytes: bytes("Zswap"),
        swap_used_bytes: swap_used,
        swap_out_pages: vmstat.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "pswpout")
                .then(|| fields.next()?.parse().ok())
                .flatten()
        }),
        load_average_one_minute: loadavg
            .split_whitespace()
            .next()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|load| load.is_finite() && *load >= 0.0),
        logical_cpus: cpus.filter(|cpus| *cpus > 0),
        unavailable: vec!["thermal_pressure".into()],
        ..HostResources::default()
    }
}

/// Apply the process's cgroup v2 memory limit: inside a container `MemAvailable` describes the
/// host, so admission could approve a load the container's OOM killer then ends.
#[cfg(any(target_os = "linux", test))]
fn apply_cgroup_limit(observed: &mut HostResources, max: &str, current: &str) {
    let (Ok(limit), Ok(used)) = (max.trim().parse::<u64>(), current.trim().parse::<u64>()) else {
        return; // "max" = unlimited, or unreadable
    };
    if observed
        .total_memory_bytes
        .is_some_and(|total| limit >= total)
    {
        return;
    }
    let headroom = limit.saturating_sub(used);
    observed.cgroup_memory_limit_bytes = Some(limit);
    observed.total_memory_bytes = Some(
        observed
            .total_memory_bytes
            .map_or(limit, |total| total.min(limit)),
    );
    observed.available_memory_bytes = Some(
        observed
            .available_memory_bytes
            .map_or(headroom, |available| available.min(headroom)),
    );
    observed.available_memory_kind = "cgroup_v2_limit_minus_usage".into();
}

/// Linux pressure-stall information as an OS memory-pressure level. `some` counts time at least
/// one task stalled on memory; `full` counts time all non-idle tasks stalled (thrashing).
#[cfg(any(target_os = "linux", test))]
fn parse_memory_psi(text: &str) -> Option<(f64, MemoryPressure)> {
    let avg10 = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))?
            .split_whitespace()
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    let some = avg10("some")?;
    let full = avg10("full").unwrap_or(0.0);
    let level = if full >= 10.0 {
        MemoryPressure::Critical
    } else if some >= 10.0 || full >= 2.0 {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Normal
    };
    Some((some, level))
}

/// `nvidia-smi --query-gpu=memory.total,memory.used --format=csv,noheader,nounits` (MiB per GPU).
#[cfg(any(target_os = "linux", test))]
fn parse_nvidia_smi(text: &str) -> Option<(u64, u64)> {
    const MIB: u64 = 1024 * 1024;
    let mut total = 0_u64;
    let mut used = 0_u64;
    let mut devices = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line
            .split(',')
            .map(|field| field.trim().parse::<u64>().ok());
        let (Some(Some(device_total)), Some(Some(device_used))) = (fields.next(), fields.next())
        else {
            return None;
        };
        total = total.checked_add(device_total.checked_mul(MIB)?)?;
        used = used.checked_add(device_used.checked_mul(MIB)?)?;
        devices += 1;
    }
    (devices > 0).then_some((total, total.saturating_sub(used)))
}

#[cfg(target_os = "linux")]
fn sample_gpu_linux(observed: &mut HostResources, read: impl Fn(&str) -> Option<String>) {
    if let Some((total, free)) = bounded_command_with(
        "nvidia-smi",
        &[
            "--query-gpu=memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_millis(1500),
    )
    .as_deref()
    .and_then(parse_nvidia_smi)
    {
        observed.gpu_memory_total_bytes = Some(total);
        observed.gpu_memory_free_bytes = Some(free);
        observed.gpu_telemetry_source = Some("nvidia_smi".into());
        return;
    }
    // amdgpu exposes VRAM counters per DRM card; integrated APUs report a small carve-out.
    let mut total = 0_u64;
    let mut used = 0_u64;
    for card in 0..8 {
        let base = format!("/sys/class/drm/card{card}/device");
        let (Some(card_total), Some(card_used)) = (
            read(&format!("{base}/mem_info_vram_total"))
                .and_then(|text| text.trim().parse::<u64>().ok()),
            read(&format!("{base}/mem_info_vram_used"))
                .and_then(|text| text.trim().parse::<u64>().ok()),
        ) else {
            continue;
        };
        total = total.saturating_add(card_total);
        used = used.saturating_add(card_used);
    }
    if total > 0 {
        observed.gpu_memory_total_bytes = Some(total);
        observed.gpu_memory_free_bytes = Some(total.saturating_sub(used));
        observed.gpu_telemetry_source = Some("amdgpu_sysfs".into());
    }
}

#[cfg(target_os = "linux")]
fn sample_linux() -> HostResources {
    let read = |path: &str| {
        std::fs::File::open(path).ok().and_then(|file| {
            let mut text = String::new();
            file.take(64 * 1024).read_to_string(&mut text).ok()?;
            Some(text)
        })
    };
    let mut observed = parse_linux(
        &read("/proc/meminfo").unwrap_or_default(),
        &read("/proc/loadavg").unwrap_or_default(),
        &read("/proc/vmstat").unwrap_or_default(),
        std::thread::available_parallelism()
            .ok()
            .and_then(|cpus| u32::try_from(cpus.get()).ok()),
    );
    if let Some(path) = read("/proc/self/cgroup").and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("0::").map(|path| path.trim().to_owned()))
    }) {
        let base = format!(
            "/sys/fs/cgroup{}",
            if path == "/" { "" } else { path.as_str() }
        );
        if let (Some(max), Some(current)) = (
            read(&format!("{base}/memory.max")),
            read(&format!("{base}/memory.current")),
        ) {
            apply_cgroup_limit(&mut observed, &max, &current);
        }
    }
    match read("/proc/pressure/memory")
        .as_deref()
        .and_then(parse_memory_psi)
    {
        Some((some, level)) => {
            observed.memory_psi_some_avg10 = Some(some);
            observed.memory_pressure = Some(level);
        }
        None => observed.unavailable.push("os_memory_pressure_level".into()),
    }
    sample_gpu_linux(&mut observed, read);
    if observed.available_memory_bytes.is_none() {
        observed.unavailable.push("MemAvailable".into());
    }
    observed
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn colon_numbers(text: &str) -> BTreeMap<&str, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let value = value
                .split_whitespace()
                .next()?
                .trim_end_matches('.')
                .parse()
                .ok()?;
            Some((key.trim(), value))
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos(vmstat: &str, sysctl: &str, thermal: &str) -> HostResources {
    let values = colon_numbers(vmstat);
    let sys: BTreeMap<_, _> = sysctl
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    let page_size = vmstat
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0);
    let bytes = |name| {
        values
            .get(name)
            .zip(page_size)
            .and_then(|(pages, size)| pages.checked_mul(size))
    };
    let total = sys
        .get("hw.memsize")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let free = bytes("Pages free");
    // vm_stat subtracts speculative pages from its printed free count. File-backed, inactive,
    // speculative and purgeable counters overlap; summing all of them would inflate headroom.
    // Use a bounded reclaimable estimate, explicitly distinct from Linux's kernel MemAvailable.
    let file_reclaimable = bytes("Pages inactive")
        .zip(bytes("Pages speculative"))
        .and_then(|(inactive, speculative)| inactive.checked_add(speculative))
        .zip(bytes("File-backed pages"))
        .map(|(inactive_and_speculative, file)| inactive_and_speculative.min(file));
    let reclaimable = file_reclaimable
        .zip(bytes("Pages purgeable"))
        .map(|(file, purgeable)| file.max(purgeable));
    let available = free
        .zip(reclaimable)
        .and_then(|(free, reclaimable)| free.checked_add(reclaimable))
        .map(|available| total.map_or(available, |total| available.min(total)));
    let speed_limit = thermal
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "CPU_Speed_Limit")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
        .filter(|value| *value <= 100);
    let thermal_warning = thermal
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            key.trim()
                .ends_with("Thermal Warning Level")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        });
    HostResources {
        source: "macos_vm_stat_sysctl_pmset".into(),
        total_memory_bytes: total,
        available_memory_bytes: available,
        available_memory_kind: "free_plus_bounded_reclaimable_estimate".into(),
        free_memory_bytes: free,
        reclaimable_memory_bytes: reclaimable,
        occupied_memory_bytes: total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available)),
        compressor_bytes: bytes("Pages occupied by compressor"),
        swap_used_bytes: sys
            .get("vm.swapusage")
            .and_then(|text| text.split("used =").nth(1))
            .and_then(|text| text.split_whitespace().next())
            .and_then(parse_memory_size),
        swap_out_pages: values.get("Swapouts").copied(),
        memory_pressure: sys
            .get("kern.memorystatus_vm_pressure_level")
            .and_then(|value| match *value {
                "1" => Some(MemoryPressure::Normal),
                "2" => Some(MemoryPressure::Warning),
                "4" => Some(MemoryPressure::Critical),
                _ => None,
            }),
        load_average_one_minute: sys
            .get("vm.loadavg")
            .and_then(|text| text.trim_start_matches('{').split_whitespace().next())
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|load| load.is_finite() && *load >= 0.0),
        logical_cpus: sys
            .get("hw.logicalcpu")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|cpus| *cpus > 0),
        thermal_throttled: if thermal_warning.is_some_and(|level| level > 0) {
            Some(true)
        } else {
            speed_limit
                .map(|limit| limit < 100)
                .or_else(|| thermal_warning.map(|_| false))
        },
        cpu_speed_limit_percent: speed_limit,
        thermal_warning_level: thermal_warning,
        ..HostResources::default()
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_size(value: &str) -> Option<u64> {
    let (number, multiplier) = match value.as_bytes().last()? {
        b'K' => (&value[..value.len() - 1], 1024_u64),
        b'M' => (&value[..value.len() - 1], 1024_u64.pow(2)),
        b'G' => (&value[..value.len() - 1], 1024_u64.pow(3)),
        _ => (value, 1),
    };
    // Fixed decimal parsing avoids lossy float-to-integer conversion of byte counters.
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(multiplier)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .ok()?
            .checked_mul(multiplier)?
            .checked_div(10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?)?
    };
    whole.checked_add(fraction)
}

#[cfg(target_os = "macos")]
fn sample_macos() -> HostResources {
    let vm = bounded_command("/usr/bin/vm_stat", &[]);
    let sys = bounded_command(
        "/usr/sbin/sysctl",
        &[
            "hw.memsize",
            "hw.logicalcpu",
            "vm.loadavg",
            "vm.swapusage",
            "kern.memorystatus_vm_pressure_level",
        ],
    );
    let therm = bounded_command("/usr/bin/pmset", &["-g", "therm"]);
    let mut result = parse_macos(
        vm.as_deref().unwrap_or_default(),
        sys.as_deref().unwrap_or_default(),
        therm.as_deref().unwrap_or_default(),
    );
    for (name, missing) in [
        ("vm_stat", vm.is_none()),
        ("sysctl", sys.is_none()),
        ("pmset", therm.is_none()),
        ("thermal_pressure", result.thermal_throttled.is_none()),
        ("available_memory", result.available_memory_bytes.is_none()),
    ] {
        if missing {
            result.unavailable.push(name.into());
        }
    }
    result
}

#[cfg(any(target_os = "macos", test))]
fn bounded_command(program: &str, args: &[&str]) -> Option<String> {
    bounded_command_with(program, args, Duration::from_millis(400))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn bounded_command_with(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout while the child runs: reading only after exit let a child with more than a
    // pipe buffer of output block on write until the deadline killed it.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(64 * 1024).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        bytes
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
    let text = String::from_utf8(reader.join().ok()?).ok()?;
    // A missing optional sysctl may cause nonzero status while the other fields remain valid.
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn healthy() -> HostResources {
        HostResources {
            source: "fixture".into(),
            total_memory_bytes: Some(32 * GIB),
            available_memory_bytes: Some(16 * GIB),
            logical_cpus: Some(8),
            load_average_one_minute: Some(2.0),
            memory_pressure: Some(MemoryPressure::Normal),
            thermal_throttled: Some(false),
            swap_out_pages: Some(100),
            ..HostResources::default()
        }
    }
    /// Pins the Linux/macOS default so these contracts hold on Windows too, whose default is
    /// best-effort.
    fn quick_policy() -> ResourcePolicy {
        ResourcePolicy {
            sample_interval: Duration::from_millis(5),
            telemetry_policy: TelemetryPolicy::RequireMemory,
            ..ResourcePolicy::default()
        }
    }

    #[tokio::test]
    async fn default_policy_denies_unknown_memory_even_for_zero_byte_requests() {
        let governor =
            ResourceGovernor::with_sampler(quick_policy(), HostResources::default).unwrap();
        let result = governor
            .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(15))
            .await;
        assert!(
            result.is_err(),
            "unknown RAM must not authorize local dispatch"
        );
        let error = result.unwrap_err();
        let assessment = error.receipt.assessment.unwrap();
        assert_eq!(
            assessment.denial_reason,
            Some(CapacityDenialReason::TelemetryUnavailable)
        );
        assert_eq!(
            assessment.missing_telemetry,
            vec![TelemetryMetric::AvailableMemory]
        );
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    }

    #[tokio::test]
    async fn low_memory_hold_admits_resident_runners_but_not_new_loads() {
        // 32 GiB unified host with a 26 GiB model resident: available memory is below the hold
        // reserve because of the model itself. Its own requests must not be locked out.
        let governor = ResourceGovernor::with_sampler(quick_policy(), || {
            let mut sample = healthy();
            sample.available_memory_bytes = Some(3 * GIB);
            sample
        })
        .unwrap();
        let permit = governor
            .wait_for_demand(
                "http://localhost:11434",
                ResourceDemand::resident(),
                Duration::from_millis(20),
            )
            .await
            .expect("resident request admitted under low-memory hold");
        assert_eq!(permit.reserved_bytes(), 0);
        let cold = governor
            .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(
            cold.receipt.assessment.unwrap().denial_reason,
            Some(CapacityDenialReason::HostPressure)
        );
    }

    #[tokio::test]
    async fn resident_runners_still_wait_for_os_pressure_and_swapping() {
        let governor = ResourceGovernor::with_sampler(quick_policy(), || {
            let mut sample = healthy();
            sample.memory_pressure = Some(MemoryPressure::Critical);
            sample
        })
        .unwrap();
        assert!(
            governor
                .wait_for_demand(
                    "http://localhost:11434",
                    ResourceDemand::resident(),
                    Duration::from_millis(20),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn released_permit_returns_capacity_before_drop() {
        let governor = ResourceGovernor::with_sampler(quick_policy(), healthy).unwrap();
        let mut permit = governor
            .wait_for_capacity("http://localhost:11434", 4 * GIB, Duration::from_millis(20))
            .await
            .unwrap();
        assert_eq!(governor.reservations.load(Ordering::Acquire), 4 * GIB);
        permit.release();
        assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
        drop(permit);
        assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cgroup_limit_caps_available_memory_inside_containers() {
        let mut sample = healthy();
        apply_cgroup_limit(&mut sample, "8589934592\n", "6442450944\n");
        assert_eq!(sample.total_memory_bytes, Some(8 * GIB));
        assert_eq!(sample.available_memory_bytes, Some(2 * GIB));
        assert_eq!(sample.cgroup_memory_limit_bytes, Some(8 * GIB));
        let mut unlimited = healthy();
        apply_cgroup_limit(&mut unlimited, "max\n", "6442450944\n");
        assert_eq!(unlimited.available_memory_bytes, Some(16 * GIB));
        let mut above_ram = healthy();
        apply_cgroup_limit(&mut above_ram, &(64 * GIB).to_string(), "0");
        assert_eq!(above_ram.cgroup_memory_limit_bytes, None);
    }

    #[test]
    fn memory_psi_maps_stalls_to_pressure_levels() {
        let calm = "some avg10=0.50 avg60=0.10 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0";
        assert_eq!(parse_memory_psi(calm), Some((0.5, MemoryPressure::Normal)));
        let stalling = "some avg10=12.00 avg60=3.00 avg300=1.00 total=1\nfull avg10=1.00 avg60=0.00 avg300=0.00 total=0";
        assert_eq!(
            parse_memory_psi(stalling).unwrap().1,
            MemoryPressure::Warning
        );
        let thrashing = "some avg10=40.00 avg60=3.00 avg300=1.00 total=1\nfull avg10=15.00 avg60=0.00 avg300=0.00 total=0";
        assert_eq!(
            parse_memory_psi(thrashing).unwrap().1,
            MemoryPressure::Critical
        );
        assert_eq!(parse_memory_psi(""), None);
    }

    #[test]
    fn nvidia_smi_sums_devices_and_rejects_garbage() {
        assert_eq!(
            parse_nvidia_smi("24576, 1024\n24576, 23552\n"),
            Some((48 * GIB, 24 * GIB))
        );
        assert_eq!(parse_nvidia_smi("[N/A], [N/A]\n"), None);
        assert_eq!(parse_nvidia_smi(""), None);
    }

    #[tokio::test]
    async fn strict_remote_bypass_never_samples_or_reserves_local_resources() {
        let governor = ResourceGovernor::with_sampler(
            ResourcePolicy {
                telemetry_policy: TelemetryPolicy::RequireAll,
                ..quick_policy()
            },
            || panic!("remote requests must not sample local resources"),
        )
        .unwrap();
        let permit = governor
            .wait_for_capacity("https://remote.example", u64::MAX, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(permit.receipt.status, "bypassed_remote");
        assert!(permit.receipt.assessment.is_none());
        assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn unknown_to_known_sample_allows_waiter_without_leaking_reservations() {
        let calls = AtomicUsize::new(0);
        let governor = ResourceGovernor::with_sampler(quick_policy(), move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                HostResources::default()
            } else {
                healthy()
            }
        })
        .unwrap();
        let permit = governor
            .wait_for_capacity("http://localhost:11434", GIB, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(permit.receipt.snapshot.as_ref().unwrap().sample_count, 2);
        assert_eq!(permit.receipt.reserved_bytes, GIB);
        drop(permit);
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    }

    #[tokio::test]
    async fn memory_policy_accepts_unavailable_thermal_but_all_policy_rejects_it() {
        let mut observation = healthy();
        observation.thermal_throttled = None;
        let governor =
            ResourceGovernor::with_sampler(quick_policy(), move || observation.clone()).unwrap();
        let mut snapshot = governor.snapshot().await;
        assert!(snapshot.assess_capacity(GIB, false).admissible);
        snapshot.policy.telemetry_policy = TelemetryPolicy::RequireAll;
        let assessment = snapshot.assess_capacity(GIB, false);
        assert!(!assessment.admissible);
        assert_eq!(
            assessment.missing_telemetry,
            vec![TelemetryMetric::ThermalPressure]
        );
        snapshot.observation.source = "linux_proc".into();
        snapshot.observation.memory_pressure = None;
        assert!(snapshot.assess_capacity(GIB, false).admissible);
        snapshot.observation.logical_cpus = None;
        assert_eq!(
            snapshot.assess_capacity(GIB, false).missing_telemetry,
            vec![TelemetryMetric::CpuLoad]
        );
    }

    #[tokio::test]
    async fn unknown_memory_recovers_and_zero_wait_revalidates_current_snapshot() {
        let known = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let switch = known.clone();
        let governor = ResourceGovernor::with_sampler(quick_policy(), move || {
            if switch.load(Ordering::SeqCst) {
                healthy()
            } else {
                HostResources::default()
            }
        })
        .unwrap();
        governor.snapshot().await;
        assert!(
            governor
                .wait_for_capacity("http://localhost:11434", 0, Duration::ZERO)
                .await
                .is_err()
        );
        known.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(7)).await;
        governor.snapshot().await;
        let permit = governor
            .wait_for_capacity("http://localhost:11434", GIB, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(permit.receipt.status, "admitted");
        drop(permit);
        known.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(7)).await;
        governor.snapshot().await;
        assert!(
            governor
                .wait_for_capacity("http://localhost:11434", 0, Duration::ZERO)
                .await
                .is_err()
        );
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    }

    #[tokio::test]
    async fn pure_assessment_accounts_for_requirements_reserves_and_overflow_without_reserving() {
        let governor = ResourceGovernor::with_sampler(quick_policy(), healthy).unwrap();
        let snapshot = governor.snapshot().await;
        assert!(snapshot.assess_capacity(GIB, false).admissible);
        assert_eq!(
            snapshot.assess_capacity(16 * GIB, false).denial_reason,
            Some(CapacityDenialReason::InsufficientCapacity)
        );
        assert_eq!(
            snapshot.assess_capacity(u64::MAX, false).denial_reason,
            Some(CapacityDenialReason::ArithmeticOverflow)
        );
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
        let permit = governor
            .wait_for_capacity("http://localhost:11434", 8 * GIB, Duration::from_millis(50))
            .await
            .unwrap();
        let reserved = governor.snapshot().await;
        assert!(!reserved.assess_capacity(8 * GIB, false).admissible);
        assert_eq!(governor.snapshot().await.reserved_bytes, 8 * GIB);
        drop(permit);
    }

    #[tokio::test]
    async fn best_effort_cannot_clear_observed_pressure_with_unknown_telemetry() {
        let mut low = healthy();
        low.available_memory_bytes = Some(GIB);
        let samples = Arc::new(std::sync::Mutex::new(vec![HostResources::default(), low]));
        let governor = ResourceGovernor::with_sampler(
            ResourcePolicy {
                telemetry_policy: TelemetryPolicy::BestEffort,
                ..quick_policy()
            },
            move || samples.lock().unwrap().pop().unwrap_or_default(),
        )
        .unwrap();
        assert!(governor.snapshot().await.holding);
        tokio::time::sleep(Duration::from_millis(7)).await;
        let snapshot = governor.snapshot().await;
        assert_eq!(
            snapshot.assess_capacity(0, false).denial_reason,
            Some(CapacityDenialReason::HostPressure)
        );
        assert!(
            governor
                .wait_for_capacity("http://localhost:11434", 0, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[test]
    fn linux_uses_memavailable_and_does_not_count_historical_swap_as_pressure() {
        let sample = parse_linux(
            "MemTotal: 32000000 kB\nMemFree: 100 kB\nMemAvailable: 16000000 kB\nSwapTotal: 8000000 kB\nSwapFree: 1000000 kB\nZswap: 123 kB",
            "2.0 1.0 1.0",
            "pswpout 500",
            Some(8),
        );
        assert_eq!(sample.available_memory_bytes, Some(16_000_000 * 1024));
        assert_eq!(sample.compressor_bytes, Some(123 * 1024));
        assert!(pressure_reasons(&sample, Some(&sample), &ResourcePolicy::default()).is_empty());
        assert_eq!(
            parse_linux("MemFree: 1 kB", "NaN", "", None).available_memory_bytes,
            None
        );
    }

    #[test]
    fn macos_reclaimable_counters_do_not_double_count_and_compressor_is_physical() {
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages inactive: 40.\nPages speculative: 20.\nFile-backed pages: 50.\nPages purgeable: 15.\nPages stored in compressor: 1000.\nPages occupied by compressor: 25.\nSwapouts: 100.";
        let sys = "hw.memsize: 34359738368\nhw.logicalcpu: 12\nvm.loadavg: { 2.34 3.05 4.68 }\nvm.swapusage: total = 11264.00M  used = 9785.88M  free = 1478.12M\nkern.memorystatus_vm_pressure_level: 1";
        let sample = parse_macos(vm, sys, "Note: No CPU power status has been recorded");
        assert_eq!(sample.available_memory_bytes, Some(60 * 16384));
        assert_eq!(sample.compressor_bytes, Some(25 * 16384));
        assert_eq!(sample.memory_pressure, Some(MemoryPressure::Normal));
        assert_eq!(sample.thermal_throttled, None);
        assert!(sample.swap_used_bytes.unwrap() > 9 * GIB);
        assert_eq!(
            parse_macos(vm, sys, " CPU_Speed_Limit = 80").thermal_throttled,
            Some(true)
        );
        assert_eq!(parse_macos("", "", "").available_memory_bytes, None);
    }

    #[tokio::test]
    async fn pressure_requires_two_fresh_recovery_samples_and_unknown_does_not_clear_it() {
        let mut low = healthy();
        low.available_memory_bytes = Some(GIB);
        let mut marginal = healthy();
        marginal.available_memory_bytes = Some(5 * GIB);
        let samples = Arc::new(std::sync::Mutex::new(vec![
            healthy(),
            healthy(),
            HostResources::default(),
            marginal,
            low,
        ]));
        let governor = ResourceGovernor::with_sampler(quick_policy(), move || {
            samples.lock().unwrap().pop().unwrap()
        })
        .unwrap();
        for expected in [true, true, true, true, false] {
            assert_eq!(governor.snapshot().await.holding, expected);
            tokio::time::sleep(Duration::from_millis(7)).await;
        }
    }

    #[tokio::test]
    async fn recovered_cpu_pressure_clears_while_memory_pressure_remains() {
        let calls = Arc::new(AtomicUsize::new(0));
        let governor = ResourceGovernor::with_sampler(quick_policy(), move || {
            let mut sample = healthy();
            sample.available_memory_bytes = Some(GIB);
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                sample.load_average_one_minute = Some(20.0);
            }
            sample
        })
        .unwrap();
        let initial = governor.snapshot().await;
        assert!(initial.reasons.contains(&PressureReason::HighCpuLoad));
        governor.invalidate().await;
        assert!(
            governor
                .snapshot()
                .await
                .reasons
                .contains(&PressureReason::HighCpuLoad),
            "one fresh recovery sample must not clear CPU pressure"
        );
        governor.invalidate().await;
        let recovered = governor.snapshot().await;
        assert_eq!(recovered.reasons, vec![PressureReason::LowAvailableMemory]);
        assert!(recovered.holding, "cold loads still need memory recovery");
        assert!(
            recovered
                .assess_demand(ResourceDemand::resident(), false)
                .admissible,
            "an old CPU-pressure signal must not block an unchanged resident runner"
        );
        assert!(!recovered.assess_capacity(GIB, false).admissible);
    }

    #[tokio::test]
    async fn concurrent_reservations_are_bounded_and_drop_releases_capacity() {
        let governor = ResourceGovernor::with_sampler(quick_policy(), healthy).unwrap();
        let first = governor
            .wait_for_capacity(
                "http://localhost:11434",
                8 * GIB,
                Duration::from_millis(100),
            )
            .await
            .unwrap();
        assert_eq!(governor.snapshot().await.reserved_bytes, 8 * GIB);
        let error = governor
            .clone()
            .wait_for_capacity("http://127.0.0.1:11434", 8 * GIB, Duration::from_millis(20))
            .await
            .err()
            .unwrap();
        assert_eq!(error.receipt.status, "deadline_exceeded");
        drop(first);
        assert_eq!(governor.snapshot().await.reserved_bytes, 0);
        assert!(
            governor
                .wait_for_capacity("http://[::1]:11434", 8 * GIB, Duration::from_millis(50))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn thermal_and_load_pressure_hold_then_recover_without_chattering() {
        let mut hot = healthy();
        hot.thermal_throttled = Some(true);
        hot.load_average_one_minute = Some(20.0);
        let samples = Arc::new(std::sync::Mutex::new(vec![healthy(), healthy(), hot]));
        let governor = ResourceGovernor::with_sampler(quick_policy(), move || {
            samples.lock().unwrap().pop().unwrap_or_else(healthy)
        })
        .unwrap();
        let permit = governor
            .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(100))
            .await
            .unwrap();
        assert!(permit.receipt.waited_ms >= 10);
        assert_eq!(permit.receipt.snapshot.as_ref().unwrap().sample_count, 3);
    }

    #[tokio::test]
    async fn remote_bypasses_sampling_and_unknown_is_explicit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let governor = ResourceGovernor::with_sampler(
            ResourcePolicy {
                telemetry_policy: TelemetryPolicy::BestEffort,
                ..quick_policy()
            },
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                HostResources::default()
            },
        )
        .unwrap();
        assert_eq!(
            governor
                .wait_for_capacity("https://remote.example", GIB, Duration::ZERO)
                .await
                .unwrap()
                .receipt
                .status,
            "bypassed_remote"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let permit = governor
            .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(permit.receipt.status, "admitted_telemetry_unknown");
        assert_eq!(permit.receipt.snapshot.as_ref().unwrap().status, "unknown");
    }

    #[tokio::test]
    async fn cancelled_sample_is_singleflight_and_wait_is_bounded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let governor = ResourceGovernor::with_sampler(ResourcePolicy::default(), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            healthy()
        })
        .unwrap();
        assert!(
            governor
                .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(5))
                .await
                .is_err()
        );
        let (a, b) = tokio::join!(governor.snapshot(), governor.snapshot());
        assert_eq!(a.sample_count, 1);
        assert_eq!(b.sample_count, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn malformed_policy_and_swap_counter_reset_are_safe() {
        let policy = ResourcePolicy {
            sample_interval: Duration::ZERO,
            ..ResourcePolicy::default()
        };
        assert!(ResourceGovernor::new(policy).is_err());
        let mut before = healthy();
        before.swap_out_pages = Some(999);
        assert_eq!(swap_grew(&healthy(), Some(&before)), Some(false));
        assert_eq!(parse_memory_size("9785.88M"), Some(10_261_238_906));
        assert_eq!(parse_memory_size("NaNM"), None);
    }

    #[test]
    fn os_warning_and_new_swap_growth_hold_without_treating_stale_swap_as_pressure() {
        let previous = healthy();
        let mut sample = healthy();
        sample.memory_pressure = Some(MemoryPressure::Warning);
        assert!(
            pressure_reasons(&sample, Some(&previous), &ResourcePolicy::default())
                .contains(&PressureReason::OsMemoryPressure)
        );
        sample.memory_pressure = Some(MemoryPressure::Normal);
        sample.available_memory_bytes = Some(5 * GIB);
        sample.swap_out_pages = Some(101);
        assert!(
            pressure_reasons(&sample, Some(&previous), &ResourcePolicy::default())
                .contains(&PressureReason::ActiveSwapping)
        );
        assert!(
            !pressure_reasons(&sample, Some(&sample), &ResourcePolicy::default())
                .contains(&PressureReason::ActiveSwapping)
        );
    }

    #[test]
    fn reservation_check_is_atomic_across_concurrent_backends() {
        let governor = ResourceGovernor::with_sampler(ResourcePolicy::default(), healthy).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let governor = governor.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    governor.try_reserve(3 * GIB, Some(16 * GIB), 5 * GIB)
                })
            })
            .collect();
        let admitted = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, 3);
        assert_eq!(governor.reservations.load(Ordering::Acquire), 9 * GIB);
    }

    #[test]
    fn telemetry_subprocess_is_killed_at_its_deadline() {
        let started = Instant::now();
        assert!(bounded_command("/bin/sleep", &["2"]).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[ignore = "read-only live host telemetry smoke; run explicitly"]
    fn live_host_resource_observation() {
        let started = Instant::now();
        let sample = sample_host_resources();
        eprintln!("{}", serde_json::to_string_pretty(&sample).unwrap());
        assert!(started.elapsed() < Duration::from_secs(3));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(sample.total_memory_bytes.is_some_and(|bytes| bytes > 0));
    }
}
