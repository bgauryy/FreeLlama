use super::governor::Reservation;
use super::pressure::{pressure_reasons, swap_grew};
use super::sampling::{
    apply_cgroup_limit, apply_thermal_pressure, bounded_command, parse_linux, parse_macos,
    parse_memory_psi, parse_memory_size, parse_nvidia_smi,
};
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Instant;

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
/// Pins the Linux/macOS telemetry default so these contracts hold on Windows too, whose default
/// is best-effort, and pins the advisory low-memory mode (the macOS default) on every platform.
fn quick_policy() -> ResourcePolicy {
    ResourcePolicy {
        sample_interval: Duration::from_millis(5),
        telemetry_policy: TelemetryPolicy::RequireMemory,
        low_memory: LowMemoryMode::Advisory,
        ..ResourcePolicy::default()
    }
}

fn refusing_policy() -> ResourcePolicy {
    ResourcePolicy {
        low_memory: LowMemoryMode::Refuse,
        ..quick_policy()
    }
}

#[test]
fn low_memory_mode_defaults_to_advisory_only_on_macos() {
    let expected = if cfg!(target_os = "macos") {
        LowMemoryMode::Advisory
    } else {
        LowMemoryMode::Refuse
    };
    assert_eq!(LowMemoryMode::default(), expected);
    assert_eq!(ResourcePolicy::default().low_memory, expected);
}

#[tokio::test]
async fn refuse_mode_holds_a_load_that_does_not_fit_and_still_admits_resident_runners() {
    let governor = ResourceGovernor::with_sampler(refusing_policy(), healthy).unwrap();
    let snapshot = governor.snapshot().await;
    let short = snapshot.assess_capacity(16 * GIB, false);
    assert!(!short.admissible);
    assert_eq!(
        short.denial_reason,
        Some(CapacityDenialReason::InsufficientCapacity)
    );
    assert_eq!(short.status, "held_insufficient_capacity");
    assert_eq!(short.memory_advisory, None);
    assert!(snapshot.assess_capacity(GIB, false).admissible);
    let error = governor
        .wait_for_capacity(
            "http://localhost:11434",
            16 * GIB,
            Duration::from_millis(20),
        )
        .await
        .expect_err("a load beyond the reserve is refused in refuse mode");
    assert_eq!(error.receipt.status, "deadline_exceeded");
    assert_eq!(error.receipt.reserved_bytes, 0);
    let assessment = error.receipt.assessment.unwrap();
    assert_eq!(assessment.status, "held_insufficient_capacity");
    assert_eq!(
        assessment.denial_reason,
        Some(CapacityDenialReason::InsufficientCapacity)
    );
    assert_eq!(governor.reservations.load(Ordering::Acquire), 0);

    // Below the hold reserve: the low-memory hold refuses new loads but not a resident runner.
    let low = ResourceGovernor::with_sampler(refusing_policy(), || {
        let mut sample = healthy();
        sample.available_memory_bytes = Some(3 * GIB);
        sample
    })
    .unwrap();
    let permit = low
        .wait_for_demand(
            "http://localhost:11434",
            ResourceDemand::resident(),
            Duration::from_millis(20),
        )
        .await
        .expect("resident request admitted under a low-memory hold");
    assert_eq!(permit.receipt.status, "admitted");
    let cold = low
        .wait_for_capacity("http://localhost:11434", GIB, Duration::ZERO)
        .await
        .expect_err("refuse mode holds a new load under a low-memory hold");
    assert_eq!(
        cold.receipt.assessment.unwrap().denial_reason,
        Some(CapacityDenialReason::HostPressure)
    );
}

#[tokio::test]
async fn refuse_mode_waiter_wakes_when_a_permit_is_released() {
    // A long sampling interval: only the release notification can wake the waiter in time.
    let governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            sample_interval: Duration::from_secs(30),
            ..refusing_policy()
        },
        healthy,
    )
    .unwrap();
    let first = governor
        .wait_for_capacity("http://localhost:11434", 8 * GIB, Duration::ZERO)
        .await
        .unwrap();
    let waiter = governor.clone();
    let started = Instant::now();
    let second = tokio::spawn(async move {
        waiter
            .wait_for_capacity("http://localhost:11434", 8 * GIB, Duration::from_secs(10))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!second.is_finished(), "the second load waits for capacity");
    drop(first);
    let permit = second
        .await
        .unwrap()
        .expect("released capacity admits the waiter");
    assert_eq!(permit.receipt.status, "admitted");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(governor.reservations.load(Ordering::Acquire), 8 * GIB);
}

#[test]
fn missing_required_memory_telemetry_is_not_reported_as_overflow() {
    let governor = ResourceGovernor::with_sampler(quick_policy(), healthy).unwrap();
    assert_eq!(
        governor.try_reserve(GIB, None, GIB),
        Reservation::TelemetryUnavailable
    );
    assert_eq!(
        governor.try_reserve(u64::MAX, Some(GIB), GIB),
        Reservation::Overflow
    );
    assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn default_policy_denies_unknown_memory_even_for_zero_byte_requests() {
    let governor = ResourceGovernor::with_sampler(quick_policy(), HostResources::default).unwrap();
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
async fn low_memory_hold_admits_resident_runners_and_warns_new_loads() {
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
    assert_eq!(permit.receipt.status, "admitted");
    // A 2 GiB load reserves its own size on top: 4 GiB needed against 3 GiB available.
    let cold = governor
        .wait_for_capacity("http://localhost:11434", 2 * GIB, Duration::ZERO)
        .await
        .expect("a low memory estimate alone does not refuse a new load");
    assert_eq!(cold.receipt.status, "admitted_low_memory_estimate");
    assert_eq!(
        cold.receipt.assessment.as_ref().unwrap().memory_advisory,
        Some(MemoryAdvisory::BelowReserve)
    );
    assert_eq!(cold.reserved_bytes(), 2 * GIB);
}

#[tokio::test]
async fn resident_runners_still_wait_for_os_pressure() {
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
async fn swapping_hold_admits_resident_reuse_but_holds_new_loads() {
    // A large load leaves the host swapping with little free memory. The resident model it
    // loaded must stay reusable; another cold load must wait in both low-memory modes.
    for low_memory in [LowMemoryMode::Advisory, LowMemoryMode::Refuse] {
        let swap = Arc::new(AtomicU64::new(100));
        let sampler_swap = swap.clone();
        let governor = ResourceGovernor::with_sampler(
            ResourcePolicy {
                low_memory,
                ..quick_policy()
            },
            move || {
                let mut sample = healthy();
                sample.available_memory_bytes = Some(3 * GIB);
                sample.swap_out_pages = Some(sampler_swap.fetch_add(1000, Ordering::AcqRel));
                sample
            },
        )
        .unwrap();
        governor.snapshot().await;
        governor.invalidate().await;
        let snapshot = governor.snapshot().await;
        assert!(
            snapshot.reasons.contains(&PressureReason::ActiveSwapping),
            "{low_memory:?}: fixture must trigger the swapping hold"
        );
        let permit = governor
            .wait_for_demand(
                "http://localhost:11434",
                ResourceDemand::resident(),
                Duration::from_millis(20),
            )
            .await
            .expect("resident reuse admitted under a swapping hold");
        assert_eq!(permit.reserved_bytes(), 0);
        let cold = governor
            .wait_for_capacity("http://localhost:11434", GIB, Duration::from_millis(20))
            .await;
        assert!(
            cold.is_err(),
            "{low_memory:?}: a new load must wait while swapping"
        );
        assert_eq!(
            snapshot.assess_capacity(GIB, false).denial_reason,
            Some(CapacityDenialReason::HostPressure)
        );
    }
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
    assert_eq!(
        parse_nvidia_smi("24576, 1024, [N/A], 0, GPU-a\n24576, 23552, 31, [N/A], GPU-b\n"),
        Some((48 * GIB, 24 * GIB))
    );
}

#[test]
fn optional_gpu_activity_never_changes_pressure_or_required_telemetry() {
    let mut observed = healthy();
    let before = pressure_reasons(&observed, None, &ResourcePolicy::default());
    let required = observed.missing_required(TelemetryPolicy::RequireAll);
    observed.gpu_activity =
        super::super::device_activity::parse_nvidia_activity("24576, 1024, 100, 100, GPU-a\n");
    assert_eq!(
        pressure_reasons(&observed, None, &ResourcePolicy::default()),
        before
    );
    assert_eq!(
        observed.missing_required(TelemetryPolicy::RequireAll),
        required
    );
    let serialized = serde_json::to_value(HostResources::default()).unwrap();
    assert!(serialized["gpu_activity"].is_null());
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
    let short = snapshot.assess_capacity(16 * GIB, false);
    assert!(short.admissible, "a low estimate warns instead of refusing");
    assert_eq!(short.memory_advisory, Some(MemoryAdvisory::BelowReserve));
    assert_eq!(short.status, "ready_low_memory_estimate");
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
    assert_eq!(
        reserved.assess_capacity(8 * GIB, false).memory_advisory,
        Some(MemoryAdvisory::BelowReserve)
    );
    assert_eq!(governor.snapshot().await.reserved_bytes, 8 * GIB);
    drop(permit);
}

#[tokio::test]
async fn best_effort_cannot_clear_observed_pressure_with_unknown_telemetry() {
    let mut pressured = healthy();
    pressured.memory_pressure = Some(MemoryPressure::Warning);
    let samples = Arc::new(std::sync::Mutex::new(vec![
        HostResources::default(),
        pressured,
    ]));
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
    assert_eq!(
        pressure_reasons(&sample, Some(&sample), &ResourcePolicy::default()),
        Vec::<PressureReason>::new()
    );
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
    assert!(
        recovered.holding,
        "the low-memory hold persists until recovery"
    );
    assert!(
        recovered
            .assess_demand(ResourceDemand::resident(), false)
            .admissible,
        "an old CPU-pressure signal must not block an unchanged resident runner"
    );
    let cold = recovered.assess_capacity(GIB, false);
    assert!(cold.admissible);
    assert_eq!(cold.memory_advisory, Some(MemoryAdvisory::BelowReserve));
}

#[tokio::test]
async fn concurrent_reservations_accumulate_and_drop_releases_capacity() {
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
    let second = governor
        .clone()
        .wait_for_capacity("http://127.0.0.1:11434", 8 * GIB, Duration::ZERO)
        .await
        .expect("headroom beyond the estimate warns instead of refusing");
    assert_eq!(second.receipt.status, "admitted_low_memory_estimate");
    assert_eq!(governor.snapshot().await.reserved_bytes, 16 * GIB);
    drop((first, second));
    assert_eq!(governor.snapshot().await.reserved_bytes, 0);
    assert_eq!(
        governor
            .wait_for_capacity("http://[::1]:11434", 8 * GIB, Duration::ZERO)
            .await
            .unwrap()
            .receipt
            .status,
        "admitted"
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
    let policy = ResourcePolicy {
        first_sample_grace: Duration::ZERO,
        low_memory: LowMemoryMode::Advisory,
        ..ResourcePolicy::default()
    };
    let governor = ResourceGovernor::with_sampler(policy, move || {
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
fn reservation_fit_is_atomic_across_concurrent_backends() {
    let governor = ResourceGovernor::with_sampler(quick_policy(), healthy).unwrap();
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
    let fitting = workers
        .into_iter()
        .map(|worker| usize::from(worker.join().unwrap() == Reservation::Fits))
        .sum::<usize>();
    assert_eq!(fitting, 3, "each reservation sees a distinct prior total");
    assert_eq!(governor.reservations.load(Ordering::Acquire), 24 * GIB);
}

#[test]
fn refuse_mode_reserves_only_what_fits_across_concurrent_backends() {
    let governor = ResourceGovernor::with_sampler(refusing_policy(), healthy).unwrap();
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
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == Reservation::Fits)
            .count(),
        3
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Reservation::Fits | Reservation::Insufficient))
    );
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

#[cfg(unix)]
mod bounded_subprocess {
    use super::run_bounded;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn kills_and_reports_a_child_past_its_deadline() {
        let started = Instant::now();
        let error = run_bounded(&mut sh("sleep 5"), Duration::from_millis(200), 1024, false)
            .expect_err("deadline must fire");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn keeps_output_when_a_grandchild_holds_the_pipe_open() {
        let started = Instant::now();
        let output = run_bounded(
            &mut sh("echo ready; sleep 5 & exit 0"),
            Duration::from_millis(500),
            1024,
            false,
        )
        .expect("child exits normally");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready\n");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn caps_output_and_drains_the_rest() {
        let output = run_bounded(
            &mut sh("head -c 200000 /dev/zero"),
            Duration::from_secs(5),
            1000,
            true,
        )
        .expect("child exits normally");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1000);
        assert_eq!(output.stderr, b"");
    }
}

/// A CPU-load hold applies to CPU-bound work and to cold loads, never to inference on a runner
/// that is already resident on the GPU: that work neither waits for nor adds to the saturated
/// cores. OS memory pressure still holds everything.
#[tokio::test]
async fn cpu_load_hold_applies_only_to_cpu_bound_work() {
    let busy = ResourceGovernor::with_sampler(quick_policy(), || {
        let mut sample = healthy();
        sample.load_average_one_minute = Some(1_000.0);
        sample.gpu_memory_total_bytes = Some(24 * GIB);
        sample
    })
    .unwrap();
    let snapshot = busy.snapshot().await;
    assert_eq!(snapshot.reasons, vec![PressureReason::HighCpuLoad]);
    let gpu_inference = snapshot.assess_demand(ResourceDemand::resident(), false);
    assert!(gpu_inference.admissible, "{gpu_inference:?}");
    assert_eq!(gpu_inference.held_by, Vec::new());
    let cpu_assigned = snapshot.assess_demand(ResourceDemand::resident().on_cpu(true), false);
    assert_eq!(
        cpu_assigned.denial_reason,
        Some(CapacityDenialReason::HostPressure)
    );
    assert_eq!(cpu_assigned.held_by, vec![PressureReason::HighCpuLoad]);
    // A GPU load is not CPU work either: a saturated host never blocks loading a GPU model (a
    // build on the same machine kept every cold load waiting), and raw passthrough, which cannot
    // know residency, is not refused for it.
    let cold_gpu = snapshot.assess_demand(ResourceDemand::load(GIB), false);
    assert_eq!(cold_gpu.held_by, Vec::new());
    assert!(cold_gpu.admissible, "{cold_gpu:?}");
    let load_on_cpu = snapshot.assess_demand(ResourceDemand::load(GIB).on_cpu(true), false);
    assert_eq!(load_on_cpu.held_by, vec![PressureReason::HighCpuLoad]);
    let raw = busy
        .wait_for_capacity("http://localhost:11434", 0, Duration::from_millis(20))
        .await
        .expect("raw passthrough admitted under a CPU-load hold");
    assert_eq!(raw.receipt.status, "admitted");
    let permit = busy
        .wait_for_demand(
            "http://localhost:11434",
            ResourceDemand::resident(),
            Duration::from_millis(20),
        )
        .await
        .expect("resident GPU inference admitted under a CPU-load hold");
    assert_eq!(permit.receipt.status, "admitted");

    // With no accelerator observed, every model decodes on the host cores, so the hold applies to
    // all work again, as on a CPU-only Linux host.
    let cpu_only = ResourceGovernor::with_sampler(quick_policy(), || {
        let mut sample = healthy();
        sample.load_average_one_minute = Some(1_000.0);
        sample
    })
    .unwrap();
    let cpu_only = cpu_only.snapshot().await;
    assert_eq!(
        cpu_only
            .assess_demand(ResourceDemand::load(GIB), false)
            .held_by,
        vec![PressureReason::HighCpuLoad]
    );
    assert_eq!(
        cpu_only
            .assess_demand(ResourceDemand::resident(), false)
            .held_by,
        vec![PressureReason::HighCpuLoad]
    );

    // Thermal throttling follows the same rule: a new load or CPU-bound work waits, a bounded
    // request on a resident GPU runner does not (waiting cools nothing it would heat).
    let throttled = ResourceGovernor::with_sampler(quick_policy(), || {
        let mut sample = healthy();
        sample.thermal_throttled = Some(true);
        sample.gpu_memory_total_bytes = Some(24 * GIB);
        sample
    })
    .unwrap();
    let hot = throttled.snapshot().await;
    assert_eq!(hot.reasons, vec![PressureReason::ThermalThrottling]);
    assert!(
        hot.assess_demand(ResourceDemand::resident(), false)
            .admissible
    );
    assert_eq!(
        hot.assess_demand(ResourceDemand::load(GIB), false).held_by,
        vec![PressureReason::ThermalThrottling]
    );
    assert_eq!(
        hot.assess_demand(ResourceDemand::resident().on_cpu(true), false)
            .held_by,
        vec![PressureReason::ThermalThrottling]
    );

    let pressured = ResourceGovernor::with_sampler(quick_policy(), || {
        let mut sample = healthy();
        sample.memory_pressure = Some(MemoryPressure::Critical);
        sample
    })
    .unwrap();
    let assessment = pressured
        .snapshot()
        .await
        .assess_demand(ResourceDemand::resident(), false);
    assert_eq!(assessment.held_by, vec![PressureReason::OsMemoryPressure]);
    assert!(!assessment.admissible);
}

/// The reserve a load must leave is capped by the load itself (above the fixed floor): a small
/// runner is not held for host headroom it will never touch, a large one keeps the full reserve.
#[tokio::test]
async fn memory_reserve_never_exceeds_the_load_it_protects() {
    let governor = ResourceGovernor::with_sampler(refusing_policy(), healthy).unwrap();
    let snapshot = governor.snapshot().await;
    let host_reserve = snapshot.hold_reserve_bytes;
    assert!(
        host_reserve > 2 * GIB,
        "the fixture host is large enough to matter: {host_reserve}"
    );
    let small = snapshot.assess_capacity(GIB / 2, false);
    assert_eq!(small.reserve_bytes, GIB, "the floor applies to a tiny load");
    let medium = snapshot.assess_capacity(2 * GIB, false);
    assert_eq!(
        medium.reserve_bytes,
        2 * GIB,
        "a mid-size load reserves its own size"
    );
    let large = snapshot.assess_capacity(host_reserve * 4, false);
    assert_eq!(
        large.reserve_bytes, host_reserve,
        "a large load keeps the host reserve"
    );
    let resident = snapshot.assess_demand(ResourceDemand::resident(), false);
    assert_eq!(resident.reserve_bytes, GIB);
}

/// Apple silicon has no pmset thermal reading; the system thermal pressure level fills it:
/// nominal and moderate are not throttled, heavy and above are, and a pmset throttle is kept.
#[test]
fn macos_thermal_pressure_fills_the_thermal_signal() {
    let mut sample = HostResources::default();
    apply_thermal_pressure(&mut sample, "com.apple.system.thermalpressurelevel 0\n");
    assert_eq!(
        (sample.thermal_pressure, sample.thermal_throttled),
        (Some("nominal"), Some(false))
    );
    apply_thermal_pressure(&mut sample, "com.apple.system.thermalpressurelevel 1");
    assert_eq!(
        (sample.thermal_pressure, sample.thermal_throttled),
        (Some("moderate"), Some(false))
    );
    apply_thermal_pressure(&mut sample, "com.apple.system.thermalpressurelevel 2");
    assert_eq!(
        (sample.thermal_pressure, sample.thermal_throttled),
        (Some("heavy"), Some(true))
    );
    let mut pmset = HostResources {
        thermal_throttled: Some(true),
        ..HostResources::default()
    };
    apply_thermal_pressure(&mut pmset, "com.apple.system.thermalpressurelevel 0");
    assert_eq!(
        pmset.thermal_throttled,
        Some(true),
        "a pmset throttle is never cleared"
    );
    let mut unknown = HostResources::default();
    apply_thermal_pressure(&mut unknown, "");
    apply_thermal_pressure(&mut unknown, "com.apple.system.thermalpressurelevel 9");
    assert_eq!(
        (unknown.thermal_pressure, unknown.thermal_throttled),
        (None, None)
    );
}
