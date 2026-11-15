use super::super::windows::parse_windows;
use super::*;

#[test]
fn windows_physical_memory_uses_checked_kib_and_preserves_unknown_metrics() {
    let observed =
        parse_windows(r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":16777216}"#);
    assert_eq!(observed.source, "windows_cim");
    assert_eq!(observed.total_memory_bytes, Some(32 * GIB));
    assert_eq!(observed.available_memory_bytes, Some(16 * GIB));
    assert_eq!(observed.free_memory_bytes, Some(16 * GIB));
    assert_eq!(observed.occupied_memory_bytes, Some(16 * GIB));
    assert_eq!(observed.reclaimable_memory_bytes, None);
    assert_eq!(observed.load_average_one_minute, None);
    assert_eq!(observed.memory_pressure, None);
    assert_eq!(observed.thermal_throttled, None);
    assert_eq!(observed.swap_used_bytes, None);
    assert_eq!(observed.gpu_memory_total_bytes, None);
    assert_eq!(
        observed.missing_required(TelemetryPolicy::RequireMemory),
        Vec::<TelemetryMetric>::new()
    );
    assert_eq!(
        observed.missing_required(TelemetryPolicy::RequireAll),
        vec![
            TelemetryMetric::CpuLoad,
            TelemetryMetric::OsMemoryPressure,
            TelemetryMetric::ThermalPressure
        ],
    );
    assert!(
        !observed
            .unavailable
            .iter()
            .any(|metric| metric == "available_memory")
    );
}

#[tokio::test]
async fn windows_missing_invalid_and_overflowing_memory_cannot_authorize_admission() {
    for text in [
        "",
        "not JSON",
        "[]",
        "{}",
        r#"{"TotalVisibleMemorySize":33554432}"#,
        r#"{"FreePhysicalMemory":16777216}"#,
        r#"{"TotalVisibleMemorySize":null,"FreePhysicalMemory":16777216}"#,
        r#"{"TotalVisibleMemorySize":0,"FreePhysicalMemory":0}"#,
        r#"{"TotalVisibleMemorySize":1,"FreePhysicalMemory":2}"#,
        r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":-1}"#,
        r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":1.5}"#,
        r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":"16777216"}"#,
        r#"{"TotalVisibleMemorySize":18446744073709551615,"FreePhysicalMemory":1}"#,
        r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":18446744073709551615}"#,
        r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":1"#,
    ] {
        let observed = parse_windows(text);
        assert_eq!(observed.available_memory_bytes, None, "{text}");
        assert!(
            observed
                .unavailable
                .iter()
                .any(|metric| metric == "available_memory")
        );
        let governor =
            ResourceGovernor::with_sampler(refusing_policy(), move || observed.clone()).unwrap();
        let assessment = governor.snapshot().await.assess_capacity(GIB, false);
        assert!(!assessment.admissible, "{text}");
        assert_eq!(
            assessment.denial_reason,
            Some(CapacityDenialReason::TelemetryUnavailable),
            "{text}"
        );
    }
}

#[tokio::test]
async fn windows_zero_available_memory_holds_a_cold_load() {
    let observed = parse_windows(r#"{"TotalVisibleMemorySize":33554432,"FreePhysicalMemory":0}"#);
    assert_eq!(observed.available_memory_bytes, Some(0));
    let governor =
        ResourceGovernor::with_sampler(refusing_policy(), move || observed.clone()).unwrap();
    let assessment = governor.snapshot().await.assess_capacity(GIB, false);
    assert_eq!(
        assessment.denial_reason,
        Some(CapacityDenialReason::InsufficientCapacity)
    );
    assert_eq!(assessment.missing_telemetry, Vec::<TelemetryMetric>::new());
}

#[test]
fn supported_platforms_require_memory_by_default() {
    if cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )) {
        assert_eq!(TelemetryPolicy::default(), TelemetryPolicy::RequireMemory);
    } else {
        assert_eq!(TelemetryPolicy::default(), TelemetryPolicy::BestEffort);
    }
}
