use super::super::GIB;
use super::super::tests::{healthy, refusing_policy};
use super::*;

#[tokio::test]
async fn poisoned_accounting_lock_still_releases_and_reserves_conservatively() {
    let governor = ResourceGovernor::with_sampler(refusing_policy(), healthy).unwrap();
    let mut permit = governor
        .wait_for_capacity("http://localhost:11434", GIB, Duration::from_secs(1))
        .await
        .unwrap();
    let accounting = Arc::clone(&governor.accounting);
    assert!(
        std::thread::spawn(move || {
            let _held = accounting.lock().unwrap();
            panic!("fault injection: accounting lock poisoning");
        })
        .join()
        .is_err()
    );

    permit.release();
    assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
    assert_eq!(
        governor.try_reserve(15 * GIB, Some(16 * GIB), 2 * GIB),
        Reservation::Insufficient
    );
    assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
    assert_eq!(
        governor.try_reserve(GIB, Some(16 * GIB), 2 * GIB),
        Reservation::Fits
    );
    assert_eq!(governor.reservations.load(Ordering::Acquire), GIB);
}

#[tokio::test]
async fn panicking_sampler_reports_unknown_memory_and_holds_admission() {
    let governor = ResourceGovernor::with_sampler(refusing_policy(), || {
        panic!("fault injection: telemetry worker panic");
    })
    .unwrap();
    let snapshot = governor.snapshot().await;
    assert_eq!(snapshot.observation.source, "sampler_failed");
    assert_eq!(snapshot.observation.available_memory_bytes, None);
    assert_eq!(
        snapshot.assess_capacity(GIB, false).denial_reason,
        Some(CapacityDenialReason::TelemetryUnavailable)
    );
    assert_eq!(governor.reservations.load(Ordering::Acquire), 0);
}
