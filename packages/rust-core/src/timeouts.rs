//! Environment-overridable request timeouts shared by the CLI, server and Node addon.

use std::time::Duration;

/// Read a timeout from the environment, falling back to `fallback` seconds.
///
/// Single source of truth for the two timeout knobs: `FREELLAMA_CONTROL_TIMEOUT_SECONDS` (30s,
/// small reads of in-memory state) and `FREELLAMA_TASK_TIMEOUT_SECONDS` (900s, a real generation).
/// The CLI, the Node addon, and the platform all read them through this function.
///
/// Zero and unparseable values fall back rather than disabling the timeout: a `Client` with no
/// timeout hangs forever against a server that accepts the connection and never answers, which is
/// the failure this exists to prevent.
#[must_use]
pub fn timeout_from_env(name: &str, fallback: u64) -> Duration {
    timeout_from_value(std::env::var(name).ok().as_deref(), fallback)
}

fn timeout_from_value(raw: Option<&str>, fallback: u64) -> Duration {
    let representable = |seconds: &u64| {
        *seconds > 0
            && std::time::Instant::now()
                .checked_add(Duration::from_secs(*seconds))
                .is_some()
    };
    Duration::from_secs(
        raw.and_then(|raw| raw.parse::<u64>().ok())
            .filter(representable)
            .or_else(|| Some(fallback).filter(representable))
            .unwrap_or(DEFAULT_TASK_TIMEOUT_SECS),
    )
}

/// Default for `FREELLAMA_CONTROL_TIMEOUT_SECONDS` — discovery calls read small in-memory state.
pub const DEFAULT_CONTROL_TIMEOUT_SECS: u64 = 30;

/// Default for `FREELLAMA_TASK_TIMEOUT_SECONDS` — a cold load plus a real generation.
pub const DEFAULT_TASK_TIMEOUT_SECS: u64 = 900;

#[cfg(test)]
mod timeout_value_tests {
    use super::*;

    #[test]
    fn deadlines_reject_unrepresentable_durations_without_panicking() {
        assert_eq!(
            timeout_from_value(Some("18446744073709551615"), 30),
            Duration::from_secs(30)
        );
        assert_eq!(timeout_from_value(Some("0"), 30), Duration::from_secs(30));
        assert_eq!(
            timeout_from_value(Some("invalid"), 30),
            Duration::from_secs(30)
        );
        assert_eq!(timeout_from_value(Some("10"), 30), Duration::from_secs(10));
        assert_eq!(
            timeout_from_value(None, u64::MAX),
            Duration::from_secs(DEFAULT_TASK_TIMEOUT_SECS)
        );
    }
}
