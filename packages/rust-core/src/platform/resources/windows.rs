//! Windows memory observation via the unprivileged CIM interface, with no native FFI.

use super::HostResources;

#[cfg(target_os = "windows")]
pub(super) fn sample_windows() -> HostResources {
    // A single CIM instance keeps total and available memory in one observation. Numeric JSON
    // avoids localized table headings and formatting. Leave room within the first-sample grace
    // period for process startup/collection; failures remain unknown rather than admitting work.
    let output = super::sampling::bounded_output(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; Get-CimInstance -ClassName Win32_OperatingSystem -Property TotalVisibleMemorySize,FreePhysicalMemory | Select-Object TotalVisibleMemorySize,FreePhysicalMemory | ConvertTo-Json -Compress",
        ],
        std::time::Duration::from_millis(1500),
    );
    let text = output
        .filter(|(success, text)| *success && (text.len() as u64) < super::TELEMETRY_OUTPUT_CAP)
        .map(|(_, text)| text);
    parse_windows(text.as_deref().unwrap_or_default())
}

/// CIM reports these two physical-memory fields in KiB. Never infer availability from total RAM,
/// page-file capacity, or a CPU/GPU model name. An invalid pair cannot authorize a load.
pub(super) fn parse_windows(text: &str) -> HostResources {
    let object = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(serde_json::Value::is_object);
    let bytes = |name| object.as_ref()?.get(name)?.as_u64()?.checked_mul(1024);
    let total = bytes("TotalVisibleMemorySize").filter(|total| *total > 0);
    let available = bytes("FreePhysicalMemory")
        .zip(total)
        .filter(|(available, total)| available <= total)
        .map(|(available, _)| available);
    let mut unavailable = vec![
        "cpu_load".into(),
        "os_memory_pressure".into(),
        "thermal_pressure".into(),
        "swap_usage".into(),
        "swap_out_pages".into(),
        "gpu_memory".into(),
        "gpu_activity".into(),
    ];
    if total.is_none() {
        unavailable.push("total_memory".into());
    }
    if available.is_none() {
        unavailable.push("available_memory".into());
    }
    HostResources {
        source: "windows_cim".into(),
        total_memory_bytes: total,
        available_memory_bytes: available,
        available_memory_kind: "windows_free_physical_memory".into(),
        free_memory_bytes: available,
        occupied_memory_bytes: total.zip(available).map(|(total, free)| total - free),
        logical_cpus: std::thread::available_parallelism()
            .ok()
            .and_then(|cpus| u32::try_from(cpus.get()).ok()),
        unavailable,
        ..HostResources::default()
    }
}
