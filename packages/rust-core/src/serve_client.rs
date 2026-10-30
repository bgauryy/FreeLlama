//! Client plumbing shared by every caller of a running `freellama serve`: the CLI and the Node
//! addon. One HTTP client, the two timeout classes, and bearer authentication from
//! `FREELLAMA_AUTH_TOKEN_FILE`, so the two front ends cannot drift apart.

use std::{path::Path, time::Duration};

use anyhow::Result;
use reqwest::{Client, RequestBuilder};

/// One process-wide HTTP client, built on first use.
///
/// `reqwest::Client` has no request timeout by default, so an endpoint that accepts the TCP
/// connection and never answers would hang a call forever. Every request therefore sets its own
/// timeout ([`control_timeout`] or [`task_timeout`]). The client owns a connection pool, a DNS
/// resolver and background driver tasks, so it is shared; a clone is a handle onto the same pool.
///
/// # Panics
///
/// Never in practice: a builder with no proxy or TLS options only fails on a missing system
/// certificate store, which [`crate::net::build_http_client`] already turns into an empty one.
#[must_use]
pub fn client() -> Client {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            crate::net::build_http_client(|builder| builder)
                .expect("an HTTP client without proxy or TLS options always builds")
        })
        .clone()
}

/// Decision-only control-plane calls: computation over an in-memory model list, so seconds means
/// wedged, not busy. Overridable via `FREELLAMA_CONTROL_TIMEOUT_SECONDS`.
#[must_use]
pub fn control_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}

/// Calls that make a model generate or load. A cold load of a large model can take minutes
/// (Ollama's own `OLLAMA_LOAD_TIMEOUT` is 5m), so this is generous. Overridable via
/// `FREELLAMA_TASK_TIMEOUT_SECONDS`.
#[must_use]
pub fn task_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_TASK_TIMEOUT_SECONDS",
        crate::DEFAULT_TASK_TIMEOUT_SECS,
    )
}

/// Attach the bearer token from `FREELLAMA_AUTH_TOKEN_FILE`, when that variable is set.
///
/// # Errors
///
/// Returns the [`crate::auth::read_token_file`] error when the token file is unsafe or invalid.
pub fn authenticate(request: RequestBuilder) -> Result<RequestBuilder> {
    let Some(path) = std::env::var_os("FREELLAMA_AUTH_TOKEN_FILE") else {
        return Ok(request);
    };
    Ok(request.bearer_auth(crate::auth::read_token_file(Path::new(&path))?))
}
