//! Upstream retry policy and the opt-in macOS Ollama restart.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::http::{HeaderMap, StatusCode};
use reqwest::Url;

use super::ProxyState;
use crate::platform::resources::run_bounded;

/// Total attempts (first try + retries) for a request that hits a transient upstream failure.
/// Matches the general-purpose default cited across retry-policy guidance for slow (LLM-scale,
/// not microservice-RPC-scale) calls: enough to ride out a brief hiccup, few enough that a
/// persistent outage still fails in bounded time.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// Exponential backoff base; attempt `n` (1-indexed) waits `RETRY_BASE_DELAY * 2^(n-1)` plus
/// jitter, so `[200ms, 400ms]` becomes the base sequence. Jitter avoids synchronized retries
/// across concurrent callers piling back onto a recovering upstream at the same instant.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

/// Upper bound on added random jitter per retry.
const RETRY_JITTER_MAX: Duration = Duration::from_millis(100);

/// Minimum time between two automatic Ollama restart attempts. Guards against a restart storm if
/// Ollama is down for an extended period and keeps failing every request — one attempt, then a
/// long cooldown before trying again, not a loop hammering `open -a Ollama`.
const RESTART_COOLDOWN: Duration = Duration::from_secs(300);

/// Deadline for each restart command; the child is killed and reaped when it passes.
const RESTART_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// `osascript` and `open` print at most a short error line.
const RESTART_OUTPUT_CAP: u64 = 64 * 1024;

/// What "restart Ollama" actually does. A trait object (not a plain fn pointer) so tests can
/// inject a closure that records the call instead of actually killing and relaunching a real
/// system process — see `with_restart_action`.
pub type RestartAction = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Quits and relaunches the macOS Ollama app. Implemented only for macOS; on other platforms it
/// logs that auto-restart is unsupported and does nothing.
pub(crate) fn default_restart_ollama() -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async {
        if !cfg!(target_os = "macos") {
            eprintln!("proxy: auto-restart is only implemented for the macOS Ollama app; skipping");
            return;
        }
        eprintln!("proxy: quitting and relaunching the Ollama app...");
        let quit = tokio::task::spawn_blocking(|| {
            run_bounded(
                std::process::Command::new("osascript").args(["-e", "quit app \"Ollama\""]),
                RESTART_COMMAND_TIMEOUT,
                RESTART_OUTPUT_CAP,
                true,
            )
        })
        .await;
        match quit {
            Ok(Ok(output)) if output.status.success() => {}
            Ok(Ok(output)) => eprintln!(
                "proxy: osascript quit failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(Err(error)) => eprintln!("proxy: osascript could not run: {error}"),
            Err(error) => eprintln!("proxy: osascript task failed: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        // `open -a` returns once Launch Services has started the app; waiting for it off the
        // runtime reaps the child instead of leaving a zombie.
        let relaunch = tokio::task::spawn_blocking(|| {
            run_bounded(
                std::process::Command::new("open").args(["-a", "Ollama"]),
                RESTART_COMMAND_TIMEOUT,
                RESTART_OUTPUT_CAP,
                true,
            )
        })
        .await;
        match relaunch {
            Ok(Ok(output)) if output.status.success() => {}
            Ok(Ok(output)) => eprintln!(
                "proxy: relaunching the Ollama app failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(Err(error)) => eprintln!("proxy: could not relaunch the Ollama app: {error}"),
            Err(error) => eprintln!("proxy: relaunch task failed: {error}"),
        }
    })
}

/// Cheap, dependency-free jitter source (not cryptographic — just enough to desynchronize
/// retries). Derived from the low bits of the system clock.
fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    RETRY_JITTER_MAX * (nanos % 1000) / 1000
}

/// Backoff before retrying `attempt` (1-indexed): `RETRY_BASE_DELAY * 2^(attempt-1)` plus jitter.
///
/// Shared with the managed-task path in `platform/mod.rs` so both retry-capable callers use one
/// backoff policy rather than drifting apart — the passthrough and the managed plane hit the same
/// Ollama, and a divergent schedule on one of them is a bug nobody would notice until it hurt.
pub(crate) fn retry_delay(attempt: u32) -> Duration {
    RETRY_BASE_DELAY * 2u32.pow(attempt.saturating_sub(1)) + jitter()
}

/// Retry 500/502 (load-model blips) and connection-establishment failures, but not 503 busy,
/// 504/client timeouts, or ambiguous failures after the upstream accepted a request.
/// A timed-out generation may still be consuming the upstream runner, so replaying it can multiply
/// load precisely when the host is under pressure. Shared with managed `/tasks` so a
/// request through the proxy cannot amplify saturation the admission semaphore is shedding.
pub(crate) fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR | StatusCode::BAD_GATEWAY
    )
}

/// Send the request, retrying transient 500/502 responses and connection-establishment errors
/// with exponential backoff. Ollama occasionally returns a 500 under load-model contention; a
/// same-request retry is enough to ride that out without surfacing an error to the caller. HTTP
/// 503, 504, client timeouts, and post-send transport failures are returned as-is.
pub(crate) async fn send_with_retries(
    state: &ProxyState,
    method: &reqwest::Method,
    target: &Url,
    headers: &HeaderMap,
    body: &axum::body::Bytes,
) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome = state
            .client
            .request(method.clone(), target.clone())
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;
        let retryable_more_attempts = attempt < MAX_ATTEMPTS;
        match outcome {
            Ok(response)
                if retryable_upstream_status(response.status()) && retryable_more_attempts =>
            {
                eprintln!(
                    "proxy retry attempt={attempt} status={} path={}",
                    response.status(),
                    target.path()
                );
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Ok(response) => return Ok(response),
            // Once the upstream accepts a request, a timeout or disconnected response may leave
            // generation running. Only failures while establishing the connection are safe to
            // replay; other transport failures would risk duplicating the accepted work.
            Err(error) if retryable_more_attempts && error.is_connect() && !error.is_timeout() => {
                eprintln!(
                    "proxy retry attempt={attempt} error={error:#} path={}",
                    target.path()
                );
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Err(error) => return Err(error).context("forward request to Ollama"),
        }
    }
}

/// True only for a genuine "nothing is listening" failure (connection refused / DNS / TLS at the
/// transport level) — never for a slow response (that's a timeout) or a live server returning an
/// error status (that's a 5xx, already handled by `send_with_retries`). Restarting Ollama is only
/// ever the right move for the first case; the other two would just add downtime for nothing.
pub(crate) fn is_connection_refused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(reqwest::Error::is_connect)
}

/// Attempts one automatic Ollama restart, gated by `RESTART_COOLDOWN` so a sustained outage
/// produces one restart attempt followed by a long quiet period, not a loop. Returns whether a
/// restart was actually attempted (the caller only retries the request if it was).
pub(crate) async fn try_restart_ollama(state: &ProxyState) -> bool {
    let mut last_attempt = state.last_restart_attempt.lock().await;
    if let Some(previous) = *last_attempt
        && previous.elapsed() < RESTART_COOLDOWN
    {
        eprintln!(
            "proxy: Ollama connection refused, but a restart was already attempted within the \
             last {}s — not attempting another one yet",
            RESTART_COOLDOWN.as_secs()
        );
        return false;
    }
    *last_attempt = Some(Instant::now());
    drop(last_attempt);
    eprintln!("proxy: Ollama connection refused — attempting one automatic restart");
    (state.restart_action)().await;
    true
}
