//! Cached host telemetry and a cooperative admission governor. It never changes OS/Ollama
//! settings or interrupts inference; a permit only accounts for additional expected memory.

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod governor;
mod holders;
mod pressure;
mod sampling;
#[cfg(test)]
mod tests;

pub use governor::{ResourceGovernor, ResourcePermit};
pub use holders::{MemoryHolder, describe as describe_holders, memory_holders};

pub(crate) use sampling::bounded_output;
pub(crate) use sampling::run_bounded;
pub use sampling::sample_host_resources;
// Only macOS callers (the doctor block and the process probe in ollama_env.rs) reach these
// through `resources`.
#[cfg(target_os = "macos")]
pub(crate) use sampling::{apply_thermal_pressure, bounded_command_with};

const GIB: u64 = 1024 * 1024 * 1024;
/// A load smaller than this is not held by active swapping alone (it still needs its own reserve).
const SMALL_LOAD_BYTES: u64 = GIB;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LowMemoryMode {
    /// Admit with a [`MemoryAdvisory`] and let Ollama and the OS decide whether the load fits.
    /// An operator choice (`--low-memory advisory`) for hosts whose available figure is known to
    /// undercount what the OS can reclaim.
    Advisory,
    /// Hold a new load (`held_insufficient_capacity`) until capacity returns or the deadline
    /// passes; work on a resident runner is never held by it. The default everywhere: on Linux
    /// `MemAvailable` counts reclaimable cache and in containers it is the hard cap, and on macOS
    /// the available figure already adds bounded reclaimable pages (inactive, speculative,
    /// purgeable), so admitting a load that does not fit pushes the host into compression and
    /// swap, which freezes an interactive machine.
    #[default]
    Refuse,
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
    /// Hold new loads that do not fit by default; `Advisory` admits them with a warning.
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

impl PressureReason {
    /// A shortage of memory, which closing other programs relieves (unlike CPU load or heat).
    #[must_use]
    pub const fn is_memory(self) -> bool {
        matches!(
            self,
            Self::LowAvailableMemory | Self::OsMemoryPressure | Self::ActiveSwapping
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceSnapshot {
    pub status: &'static str,
    pub holding: bool,
    pub reasons: Vec<PressureReason>,
    pub healthy_samples: u32,
    pub sample_count: u64,
    /// Accounting generation when this observation began; invalidated by forecast releases.
    pub sample_generation: u64,
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
    StaleObservation,
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
    /// memory pressure holds new loads and, when critical, resident runners too. For a new load, low available memory is
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
        let os_critical = self.observation.memory_pressure == Some(MemoryPressure::Critical);
        // On a host with no observed accelerator, the "GPU" backend runs on the same cores.
        let demand = if self.observation.has_accelerator() {
            demand
        } else {
            demand.on_cpu(true)
        };
        let held_by = self
            .reasons
            .iter()
            .copied()
            .filter(|reason| demand.is_held_by(*reason, advisory_mode, os_critical))
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
            Some(CapacityDenialReason::StaleObservation) => "held_stale_observation",
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
    /// and it adds to that saturation. GPU work, loaded or not, does neither, so a CPU load hold
    /// does not apply to it.
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

    /// Whether a host pressure hold applies to this demand. Active swapping never applies to a
    /// resident runner: the load that caused it must stay reusable or the host deadlocks on its own
    /// model. Low available memory holds nothing by itself; whether a new load fits its own reserve
    /// is decided by the capacity check ([`ResourceSnapshot::assess_demand`]). A CPU-load hold applies only to CPU-bound
    /// work: saturated cores slow decoding on them, not a GPU load or GPU inference. A thermal
    /// hold applies to CPU-bound work and to cold loads, a new heat source while the OS reports
    /// throttling; a bounded request on a loaded GPU runner is not held, as waiting only delays
    /// the caller. OS memory pressure holds every new load; it holds a resident runner only at
    /// the critical level, since a warning is what a large load itself raises and the runner
    /// must stay reusable for the same reason as the memory-capacity holds.
    #[must_use]
    pub fn is_held_by(
        self,
        reason: PressureReason,
        advisory_low_memory: bool,
        os_critical: bool,
    ) -> bool {
        match reason {
            // Low available memory is judged per load, not host-wide: the capacity check admits
            // a load that still leaves its own reserve (at least 1 GiB, up to the host reserve
            // for a large one) and holds one that does not. A host-wide hold here kept a 300 MB
            // embedding model out while 4 GB were free.
            PressureReason::LowAvailableMemory => {
                let _ = advisory_low_memory;
                false
            }
            // A small load of known size (under the 1 GiB minimum reserve) adds too little to
            // matter while the OS pages, and still has to fit its own reserve; anything larger, or
            // of unknown size, waits until swapping stops. Without this a build that keeps the
            // machine swapping blocked even a 300 MB embedding model.
            PressureReason::ActiveSwapping => {
                !self.resident_only() && (self.bytes == 0 || self.bytes >= SMALL_LOAD_BYTES)
            }
            // Saturated host cores slow work that decodes on them; they say nothing about a GPU
            // load or GPU inference, so a busy compiler never blocks a GPU model.
            PressureReason::HighCpuLoad => self.cpu_bound,
            PressureReason::ThermalThrottling => self.cpu_bound || !self.resident,
            PressureReason::OsMemoryPressure => os_critical || !self.resident_only(),
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
