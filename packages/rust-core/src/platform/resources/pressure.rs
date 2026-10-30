//! Pressure detection and per-signal recovery rules for resource holds.

use super::{
    HostResources, MemoryPressure, PressureReason, ResourcePolicy, TelemetryMetric, TelemetryPolicy,
};

impl HostResources {
    /// Whether this sample saw an accelerator: GPU devices (Apple I/O registry, `nvidia-smi`) or
    /// GPU memory. Without one every model decodes on the host cores, so CPU saturation applies
    /// to all work; a GPU the collectors cannot read keeps that conservative rule.
    pub(super) fn has_accelerator(&self) -> bool {
        self.gpu_memory_total_bytes.is_some()
            || self
                .gpu_activity
                .as_ref()
                .is_some_and(|activity| !activity.devices.is_empty())
    }

    pub(super) fn missing_required(&self, policy: TelemetryPolicy) -> Vec<TelemetryMetric> {
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

    pub(super) fn load_per_cpu(&self) -> Option<f64> {
        let load = self
            .load_average_one_minute
            .filter(|load| load.is_finite() && *load >= 0.0)?;
        let cores = self.logical_cpus.filter(|cores| *cores > 0)?;
        Some(load / f64::from(cores))
    }

    pub(super) fn has_signal(&self) -> bool {
        self.available_memory_bytes.is_some()
            || self.load_per_cpu().is_some()
            || self.memory_pressure.is_some()
            || self.thermal_throttled.is_some()
    }
}

pub(super) fn swap_grew(current: &HostResources, previous: Option<&HostResources>) -> Option<bool> {
    current
        .swap_out_pages
        .zip(previous?.swap_out_pages)
        .map(|(now, before)| now > before)
}

pub(super) fn pressure_reasons(
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

pub(super) fn recovered_reason(
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
pub(super) fn reason_observed(reason: PressureReason, current: &HostResources) -> bool {
    match reason {
        PressureReason::LowAvailableMemory | PressureReason::ActiveSwapping => {
            current.available_memory_bytes.is_some()
        }
        PressureReason::OsMemoryPressure => current.memory_pressure.is_some(),
        PressureReason::HighCpuLoad => current.load_per_cpu().is_some(),
        PressureReason::ThermalThrottling => current.thermal_throttled.is_some(),
    }
}
