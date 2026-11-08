//! A process-wide budget for request bodies buffered in memory.
//!
//! Raw proxy requests and control-API requests are read whole before admission (a retried attempt
//! must resend the same bytes, and a slow upload must not hold an execution lock). Each body up to
//! `FREELLAMA_MAX_REQUEST_BYTES` is fine alone, but concurrent uploads multiply it. Every request
//! reserves its expected size here before reading, and returns it when the buffered body is gone.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default for `FREELLAMA_MAX_BUFFERED_BYTES`: four maximum-size bodies at the default limit.
pub(crate) const DEFAULT_MAX_BUFFERED_BYTES: usize = 256 * 1024 * 1024;
/// The budget is counted in KiB, so `u32` permits cover four terabytes.
const UNIT: usize = 1024;
const UNIT_BYTES: u64 = 1024;

/// Bytes reserved for buffered request bodies, shared by every server in this process.
#[derive(Clone, Debug)]
pub(crate) struct BodyBudget {
    permits: Arc<Semaphore>,
    total_units: u32,
}

/// Held while a buffered body is in memory; dropping it returns the bytes.
#[derive(Debug)]
#[must_use = "the reservation is returned when this guard drops"]
pub(crate) struct BodyReservation {
    _permit: Option<OwnedSemaphorePermit>,
}

/// The budget stayed full for the whole wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyBudgetExhausted;

impl BodyBudget {
    /// A budget of `total_bytes`, rounded up to whole KiB and at least one KiB.
    #[must_use]
    pub(crate) fn new(total_bytes: usize) -> Self {
        let units = u32::try_from(total_bytes.div_ceil(UNIT))
            .unwrap_or(u32::MAX)
            .clamp(1, u32::try_from(Semaphore::MAX_PERMITS).unwrap_or(u32::MAX));
        Self {
            permits: Arc::new(Semaphore::new(units as usize)),
            total_units: units,
        }
    }

    /// The process-wide budget: `FREELLAMA_MAX_BUFFERED_BYTES`, else
    /// [`DEFAULT_MAX_BUFFERED_BYTES`]. Zero and unparseable values fall back to the default.
    #[must_use]
    pub(crate) fn shared() -> Self {
        static SHARED: OnceLock<BodyBudget> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                Self::new(
                    std::env::var("FREELLAMA_MAX_BUFFERED_BYTES")
                        .ok()
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .filter(|bytes| *bytes > 0)
                        .unwrap_or(DEFAULT_MAX_BUFFERED_BYTES),
                )
            })
            .clone()
    }

    /// Reserve `bytes`, waiting at most `wait` for other bodies to be released. An empty body
    /// reserves nothing; a body larger than the whole budget reserves all of it, so it waits for
    /// every other buffered body instead of being refused forever.
    ///
    /// # Errors
    ///
    /// [`BodyBudgetExhausted`] when the budget did not free enough room within `wait`.
    pub(crate) async fn reserve(
        &self,
        bytes: usize,
        wait: Duration,
    ) -> Result<BodyReservation, BodyBudgetExhausted> {
        let units = u32::try_from(bytes.div_ceil(UNIT))
            .unwrap_or(u32::MAX)
            .min(self.total_units);
        if units == 0 {
            return Ok(BodyReservation { _permit: None });
        }
        if let Ok(permit) = Arc::clone(&self.permits).try_acquire_many_owned(units) {
            return Ok(BodyReservation {
                _permit: Some(permit),
            });
        }
        match tokio::time::timeout(wait, Arc::clone(&self.permits).acquire_many_owned(units)).await
        {
            Ok(Ok(permit)) => Ok(BodyReservation {
                _permit: Some(permit),
            }),
            // The semaphore is never closed; a closed one has no room either.
            Ok(Err(_)) | Err(_) => Err(BodyBudgetExhausted),
        }
    }

    /// Total and currently free bytes, for health receipts.
    #[must_use]
    pub(crate) fn receipt(&self) -> serde_json::Value {
        let available = u64::try_from(self.permits.available_permits()).unwrap_or(u64::MAX);
        serde_json::json!({
            "total_bytes": u64::from(self.total_units).saturating_mul(UNIT_BYTES),
            "available_bytes": available.saturating_mul(UNIT_BYTES),
        })
    }
}

impl std::fmt::Display for BodyBudgetExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "server busy: buffered request bodies fill FREELLAMA_MAX_BUFFERED_BYTES; retry shortly",
        )
    }
}

impl std::error::Error for BodyBudgetExhausted {}

/// The bytes a request body will occupy once buffered: its exact or upper size hint (hyper
/// derives it from `Content-Length`; an empty `GET` is exactly zero), else `limit`.
pub(crate) fn expected_body_bytes(hint: &http_body::SizeHint, limit: usize) -> usize {
    hint.upper()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(limit)
        .min(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reservations_wait_for_released_bytes_and_refuse_after_the_wait() {
        let budget = BodyBudget::new(4 * UNIT);
        let first = budget.reserve(3 * UNIT, Duration::ZERO).await.unwrap();
        assert_eq!(budget.receipt()["available_bytes"], UNIT);
        assert_eq!(
            budget
                .reserve(2 * UNIT, Duration::from_millis(10))
                .await
                .unwrap_err(),
            BodyBudgetExhausted
        );
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { budget.reserve(2 * UNIT, Duration::from_secs(2)).await }
        });
        tokio::task::yield_now().await;
        drop(first);
        let second = waiting.await.unwrap().unwrap();
        assert_eq!(budget.receipt()["available_bytes"], 2 * UNIT);
        drop(second);
        assert_eq!(budget.receipt()["available_bytes"], 4 * UNIT);
    }

    #[tokio::test]
    async fn empty_bodies_reserve_nothing_and_oversized_ones_take_the_whole_budget() {
        let budget = BodyBudget::new(2 * UNIT);
        let held = budget.reserve(UNIT, Duration::ZERO).await.unwrap();
        let _empty = budget.reserve(0, Duration::ZERO).await.unwrap();
        assert_eq!(budget.receipt()["available_bytes"], UNIT);
        // One byte still costs a whole unit.
        let byte = budget.reserve(1, Duration::ZERO).await.unwrap();
        assert!(budget.reserve(1, Duration::ZERO).await.is_err());
        drop(held);
        drop(byte);
        let whole = budget
            .reserve(64 * UNIT, Duration::ZERO)
            .await
            .expect("a body larger than the budget waits for all of it, not forever");
        assert_eq!(budget.receipt()["available_bytes"], 0);
        drop(whole);
    }

    #[test]
    fn expected_size_uses_the_hint_and_falls_back_to_the_limit() {
        assert_eq!(
            expected_body_bytes(&http_body::SizeHint::with_exact(0), 64),
            0
        );
        assert_eq!(
            expected_body_bytes(&http_body::SizeHint::with_exact(10), 64),
            10
        );
        assert_eq!(
            expected_body_bytes(&http_body::SizeHint::with_exact(100), 64),
            64
        );
        assert_eq!(expected_body_bytes(&http_body::SizeHint::new(), 64), 64);
    }
}
