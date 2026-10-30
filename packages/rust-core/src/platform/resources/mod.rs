//! Cached host telemetry and a cooperative admission governor. It never changes OS/Ollama
//! settings or interrupts inference; a permit only accounts for additional expected memory.

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod governor;
mod pressure;
mod sampling;
#[cfg(test)]
mod tests;

pub use governor::{ResourceGovernor, ResourcePermit};

pub(crate) use sampling::bounded_output;
pub(crate) use sampling::run_bounded;
pub use sampling::sample_host_resources;
// Only macOS callers (the doctor block and the process probe in ollama_env.rs) reach these
// through `resources`.
#[cfg(target_os = "macos")]
pub(crate) use sampling::{apply_thermal_pressure, bounded_command_with};

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

/// Driver-defined activity for observed devices, across all processes. This is diagnostic only:
/// it neither attributes work to Ollama nor establishes complete device coverage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuActivityObservation {
    pub source: String,
    pub scope: &'static str,
    pub window: &'static str,
    pub devices: Vec<GpuDeviceActivity>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpuDeviceActivity {
    pub device: String,
    /// Missing, unsupported or malformed percentages stay unknown; observed zero is retained.
    pub busy_percent: Option<f64>,
    pub memory_busy_percent: Option<f64>,
    /// Memory the GPU driver has allocated from the shared pool (Apple `Alloc system memory`);
    /// unknown elsewhere. Unified memory is still admitted against host RAM.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_memory_bytes: Option<u64>,
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
    /// "No CPU power status has been recorded" response. On macOS the system thermal pressure
    /// level counts as throttled from `heavy` up.
    pub thermal_throttled: Option<bool>,
    /// macOS system thermal pressure (`nominal`, `moderate`, `heavy`, `trapping`, `sleeping`)
    /// from the `com.apple.system.thermalpressurelevel` notification state, readable unprivileged.
    pub thermal_pressure: Option<&'static str>,
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
    /// Optional all-process activity; never used as an admission or pressure signal.
    pub gpu_activity: Option<GpuActivityObservation>,
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

/// What admission does when a new load's estimate does not fit the available-memory reserve and
/// no OS signal reports pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LowMemoryMode {
    /// Admit with a [`MemoryAdvisory`] and let Ollama and the OS decide whether the load fits.
    /// Justified on macOS, whose available-memory figure undercounts what it can reclaim.
    Advisory,
    /// Hold the request (`held_insufficient_capacity`) until capacity returns or the deadline
    /// passes. On Linux `MemAvailable` already counts reclaimable cache and in containers it is
    /// the hard cap, so over-admitting invites the OOM killer; Windows is similar.
    Refuse,
}

impl Default for LowMemoryMode {
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Advisory
        } else {
            Self::Refuse
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
    /// Platform-dependent by default: advisory on macOS, refusing elsewhere.
    pub low_memory: LowMemoryMode,
    pub sample_interval: Duration,
    pub hold_available_percent: u32,
    pub resume_available_percent: u32,
    pub hold_available_min_bytes: u64,
    pub resume_available_min_bytes: u64,
    pub hold_load_per_cpu: f64,
    pub resume_load_per_cpu: f64,
    pub recovery_samples: u32,
    /// Bounds the first host observation of a capacity wait independently of the caller's
    /// timeout, so a zero wait still assesses one sample (the sampler runs on the blocking pool,
    /// which can lag under load).
    pub first_sample_grace: Duration,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            telemetry_policy: TelemetryPolicy::default(),
            low_memory: LowMemoryMode::default(),
            sample_interval: Duration::from_secs(2),
            hold_available_percent: 15,
            resume_available_percent: 20,
            hold_available_min_bytes: GIB,
            resume_available_min_bytes: 2 * GIB,
            hold_load_per_cpu: 1.5,
            resume_load_per_cpu: 1.0,
            recovery_samples: 2,
            first_sample_grace: Duration::from_secs(2),
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

    /// The headroom a new load of `bytes` must leave: the host reserve, but never more than the
    /// load itself above the fixed floor. A 3 GB runner on a 64 GB host is not held for 10 GB of
    /// spare RAM it will never touch (the pattern peers use is a fixed margin next to the load,
    /// not a share of the host), while a load near the host's size keeps the full reserve.
    fn reserve_for(&self, total: Option<u64>, recovering: bool, bytes: u64) -> u64 {
        let floor = if recovering {
            self.resume_available_min_bytes
        } else {
            self.hold_available_min_bytes
        };
        self.reserve(total, recovering).min(bytes.max(floor))
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
    /// [`LowMemoryMode::Refuse`]: the load does not fit the available-memory reserve.
    InsufficientCapacity,
    ArithmeticOverflow,
}

/// `FreeLlama`'s own available-memory estimate is short, but no OS signal reports pressure. Under
/// [`LowMemoryMode::Advisory`] (the macOS default, where the estimate undercounts what the OS can
/// reclaim) this warns and triggers idle-runner eviction instead of refusing work; Ollama and the
/// OS decide whether the load fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAdvisory {
    /// Available memory does not cover reservations, this load, and the reserve.
    BelowReserve,
}

/// Point-in-time capacity advice, without acquiring a permit or changing reservations.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceCapacityAssessment {
    pub status: &'static str,
    pub admissible: bool,
    pub denial_reason: Option<CapacityDenialReason>,
    pub memory_advisory: Option<MemoryAdvisory>,
    /// The active host holds that apply to this demand (empty when none holds it, even while
    /// the host holds other kinds of work).
    pub held_by: Vec<PressureReason>,
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

    /// Like [`Self::assess_capacity`], for a [`ResourceDemand`]. Which active holds deny the
    /// demand is decided per signal by [`ResourceDemand::is_held_by`]: memory-capacity holds spare a
    /// resident runner, CPU-load and thermal holds spare inference on a resident GPU runner, OS
    /// memory pressure holds everything. For a new load, low available memory is
    /// a [`MemoryAdvisory`] under [`LowMemoryMode::Advisory`] and an
    /// [`CapacityDenialReason::InsufficientCapacity`] denial under [`LowMemoryMode::Refuse`].
    #[must_use]
    pub fn assess_demand(
        &self,
        demand: ResourceDemand,
        recovering_capacity: bool,
    ) -> ResourceCapacityAssessment {
        let required_available_bytes = demand.bytes;
        let resident_only = demand.resident_only();
        let advisory_mode = self.policy.low_memory == LowMemoryMode::Advisory;
        let held_by = self
            .reasons
            .iter()
            .copied()
            .filter(|reason| demand.is_held_by(*reason, advisory_mode))
            .collect::<Vec<_>>();
        let reserve_bytes = self.policy.reserve_for(
            self.observation.total_memory_bytes,
            recovering_capacity,
            required_available_bytes,
        );
        let required_with_reserve_bytes = required_available_bytes.checked_add(reserve_bytes);
        let missing_telemetry = self
            .observation
            .missing_required(self.policy.telemetry_policy);
        let below_reserve = !resident_only
            && self.effective_available_bytes.is_some_and(|available| {
                required_with_reserve_bytes.is_some_and(|required| available < required)
            });
        let denial_reason = if self.holding && !held_by.is_empty() {
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
        } else if below_reserve && !advisory_mode {
            Some(CapacityDenialReason::InsufficientCapacity)
        } else {
            None
        };
        let memory_advisory =
            (denial_reason.is_none() && below_reserve).then_some(MemoryAdvisory::BelowReserve);
        let status = match denial_reason {
            Some(CapacityDenialReason::HostPressure) => "held_host_pressure",
            Some(CapacityDenialReason::TelemetryUnavailable) => "held_telemetry_unavailable",
            Some(CapacityDenialReason::InsufficientCapacity) => "held_insufficient_capacity",
            Some(CapacityDenialReason::ArithmeticOverflow) => "held_arithmetic_overflow",
            None if memory_advisory.is_some() => "ready_low_memory_estimate",
            None if self.observation.available_memory_bytes.is_none() => "ready_telemetry_unknown",
            None => "ready",
        };
        ResourceCapacityAssessment {
            status,
            admissible: denial_reason.is_none(),
            denial_reason,
            memory_advisory,
            held_by,
            required_available_bytes,
            reserve_bytes,
            required_with_reserve_bytes,
            effective_available_bytes: self.effective_available_bytes,
            missing_telemetry,
        }
    }
}

/// What a request adds to host memory, and which host signals can hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResourceDemand {
    pub bytes: u64,
    /// The runner is already loaded with a compatible configuration.
    pub resident: bool,
    /// The work decodes on the host CPUs (a CPU-assigned backend), so CPU saturation slows it
    /// and it adds to that saturation. A resident GPU runner's inference does neither, so a CPU
    /// load hold does not apply to it; loading weights is CPU work in both cases.
    pub cpu_bound: bool,
}

impl ResourceDemand {
    #[must_use]
    pub const fn load(bytes: u64) -> Self {
        Self {
            bytes,
            resident: false,
            cpu_bound: false,
        }
    }

    #[must_use]
    pub const fn resident() -> Self {
        Self {
            bytes: 0,
            resident: true,
            cpu_bound: false,
        }
    }

    /// The same demand executed by a CPU backend.
    #[must_use]
    pub const fn on_cpu(self, cpu_bound: bool) -> Self {
        Self { cpu_bound, ..self }
    }

    /// A resident runner that adds no memory.
    #[must_use]
    pub const fn resident_only(self) -> bool {
        self.resident && self.bytes == 0
    }

    /// Whether a host pressure hold applies to this demand. Memory-capacity holds (low available
    /// memory, active swapping) never apply to a resident runner: the load that caused them must
    /// stay reusable or the host deadlocks on its own model, and low available memory is only
    /// advisory under [`LowMemoryMode::Advisory`]. CPU-load and thermal holds apply to CPU-bound
    /// work and to cold loads (decoding weights is CPU work and a new heat source), not to
    /// inference on a resident GPU runner: a bounded request on a loaded runner neither waits for
    /// the cores nor cools the package by waiting, so holding it only delays the caller. OS memory
    /// pressure holds every demand.
    #[must_use]
    pub fn is_held_by(self, reason: PressureReason, advisory_low_memory: bool) -> bool {
        match reason {
            PressureReason::LowAvailableMemory => !advisory_low_memory && !self.resident_only(),
            PressureReason::ActiveSwapping => !self.resident_only(),
            PressureReason::HighCpuLoad | PressureReason::ThermalThrottling => {
                self.cpu_bound || !self.resident
            }
            PressureReason::OsMemoryPressure => true,
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
