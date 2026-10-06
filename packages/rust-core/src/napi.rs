//! Node-native bindings (via `napi-rs`) exposing `FreeLlama`'s local-LLM control plane as plain
//! async functions, so a Node/TypeScript MCP server can define tools that call straight into this
//! binary instead of shelling out to the CLI or hand-rolling HTTP requests.
//!
//! This module is the *only* place in the crate allowed to contain `unsafe` (see the
//! `#[allow(unsafe_code)]` on its `pub mod napi;` declaration in `lib.rs`) — `napi-derive`'s
//! generated FFI glue requires it. Every exported function here is a thin async wrapper: it either
//! calls a plain library function directly (`doctor`) or makes one HTTP call to an already-running
//! `freellama serve` instance, exactly mirroring how the CLI subcommands work
//! (`print_get`/`print_post` in `packages/cli/src/http.rs`, `request_route` etc. in
//! `packages/cli/src/requests.rs`), sharing their client, timeouts and authentication through
//! [`crate::serve_client`] — this crate does not duplicate routing
//! or model-discovery logic a second time; the running server stays the single
//! source of truth for all of it.
//!
//! `doctor` is the one exception: it's a standalone library function (`crate::doctor`) that talks
//! directly to Ollama and needs no running `freellama serve` at all.

use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::{Value, json};

use crate::{DEFAULT_OLLAMA_ENDPOINT, DEFAULT_SERVE_ENDPOINT};

/// Functions that need a running `freellama serve` default to [`DEFAULT_SERVE_ENDPOINT`],
/// overridable via `FREELLAMA_SERVE_ENDPOINT`.
fn endpoint_or_default(endpoint: Option<String>) -> String {
    endpoint
        .or_else(|| std::env::var("FREELLAMA_SERVE_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_SERVE_ENDPOINT.to_owned())
}

/// `doctor` talks to Ollama directly without `freellama serve`, so it defaults to
/// [`DEFAULT_OLLAMA_ENDPOINT`], overridable via `FREELLAMA_OLLAMA_ENDPOINT`.
fn ollama_endpoint_or_default(endpoint: Option<String>) -> String {
    endpoint
        .or_else(|| std::env::var("FREELLAMA_OLLAMA_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_owned())
}

fn to_napi_err(error: impl std::fmt::Display) -> Error {
    Error::from_reason(error.to_string())
}

/// [`crate::serve_client::authenticate`], with the error named for the JavaScript caller.
fn authenticated(request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
    crate::serve_client::authenticate(request)
        .map_err(|error| Error::from_reason(format!("FREELLAMA_AUTH_TOKEN_FILE: {error:#}")))
}

async fn get_json(endpoint: &str, path: &str, timeout: Duration) -> Result<Value> {
    request_json(
        crate::serve_client::client().get(crate::ollama_http::url(endpoint, path)),
        timeout,
    )
    .await
}

async fn post_json(endpoint: &str, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
    request_json(
        crate::serve_client::client()
            .post(crate::ollama_http::url(endpoint, path))
            .json(body),
        timeout,
    )
    .await
}

async fn checked_response(
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response> {
    let response = authenticated(request)?
        .timeout(timeout)
        .send()
        .await
        .map_err(to_napi_err)?;
    if response.status().is_success() {
        return Ok(response);
    }
    // Preserve application refusal receipts and plain-text extractor errors for every method.
    let text = response.text().await.map_err(to_napi_err)?;
    let detail = serde_json::from_str::<Value>(&text)
        .map_or_else(|_| text.clone(), |value| value.to_string());
    Err(napi::Error::from_reason(detail))
}

async fn request_json(request: reqwest::RequestBuilder, timeout: Duration) -> Result<Value> {
    let response = checked_response(request, timeout).await?;
    let status = response.status();
    let text = response.text().await.map_err(to_napi_err)?;
    serde_json::from_str::<Value>(&text).map_err(|error| {
        napi::Error::from_reason(format!(
            "FreeLlama returned HTTP {status} with invalid JSON: {error}"
        ))
    })
}

async fn delete_json(endpoint: &str, path: &str, timeout: Duration) -> Result<()> {
    checked_response(
        crate::serve_client::client().delete(crate::ollama_http::url(endpoint, path)),
        timeout,
    )
    .await?;
    Ok(())
}

fn pretty(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(to_napi_err)
}

/// Crate version this addon was compiled from; release verification requires it to match the
/// npm package version.
#[napi]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Keeps the version bytes in the compiled addon (the optimizer may inline the literal above), so
/// `scripts/release-version.mjs` can reject an addon built at another version without loading it.
#[used]
static EMBEDDED_VERSION: &str = concat!("freellama-version:", env!("CARGO_PKG_VERSION"));

/// Runs `freellama doctor` against Ollama directly — no running `freellama serve` required.
/// Cross-checks the Ollama CLI and server versions and confirms the endpoint is reachable.
///
/// # Errors
///
/// Returns an error if Ollama is unreachable at `endpoint`.
#[napi]
pub async fn doctor(endpoint: Option<String>) -> Result<String> {
    let endpoint = ollama_endpoint_or_default(endpoint);
    let report = crate::doctor(&endpoint).await.map_err(to_napi_err)?;
    pretty(&report)
}

/// Machine profile (chip, host memory, memory kind, CPU count, and disk) from `freellama serve`.
///
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or returns a non-2xx
/// response.
#[napi]
pub async fn machine(endpoint: Option<String>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = get_json(
        &endpoint,
        "/_freellama/v1/machine",
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Current managed-platform health, including admission and session bounds.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or returns a non-2xx response.
#[napi]
pub async fn health(endpoint: Option<String>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = get_json(
        &endpoint,
        "/_freellama/v1/health",
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Live platform view: per-backend admission, queues, adaptive limits and circuit breakers,
/// loaded models, host memory, Ollama's effective settings, and today's usage.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable.
#[napi]
pub async fn status(endpoint: Option<String>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = get_json(
        &endpoint,
        "/_freellama/v1/status",
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Usage totals for the last `days` days (default 7), per day and per model.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable.
#[napi]
pub async fn usage(endpoint: Option<String>, days: Option<u32>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let path = format!("/_freellama/v1/usage?days={}", days.unwrap_or(7));
    let value = get_json(&endpoint, &path, crate::serve_client::control_timeout()).await?;
    pretty(&value)
}

/// Create a bounded, expiring model-affinity handle. It stores no prompt history or Ollama KV.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or its session limit is full.
#[napi]
pub async fn create_session(endpoint: Option<String>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(
        &endpoint,
        "/_freellama/v1/sessions",
        &json!({}),
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Release a model-affinity handle when an agent has finished related work.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or the handle has expired or was deleted.
#[napi]
pub async fn delete_session(endpoint: Option<String>, session_id: String) -> Result<()> {
    let endpoint = endpoint_or_default(endpoint);
    delete_json(
        &endpoint,
        &format!("/_freellama/v1/sessions/{session_id}"),
        crate::serve_client::control_timeout(),
    )
    .await
}

/// Cancel managed requests for one session and invalidate its affinity handle.
///
/// # Errors
/// Returns an error if the server is unreachable or the session does not exist.
#[napi]
pub async fn kill_session(endpoint: Option<String>, session_id: String) -> Result<String> {
    uuid::Uuid::parse_str(&session_id)
        .map_err(|error| Error::from_reason(format!("invalid session UUID: {error}")))?;
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(
        &endpoint,
        &format!("/_freellama/v1/sessions/{session_id}/kill"),
        &json!({}),
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Installed-model inventory with capabilities, residency, and advertised context, as discovered
/// by a running `freellama serve`.
///
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or returns a non-2xx
/// response.
#[napi]
pub async fn list_models(endpoint: Option<String>) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = get_json(
        &endpoint,
        "/_freellama/v1/models",
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Deterministic model selection for a task, via `POST /_freellama/v1/routes` on a running
/// `freellama serve`. `task` and `objective` are passed through as strings and validated
/// server-side (e.g. task: `completion` | `code_repair` | `vision` | `embedding` | ...;
/// objective: `fastest` | `balanced` | `quality`).
///
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or rejects the request
/// (e.g. an unknown task/objective, or no eligible model).
#[napi]
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the positional JS signature; `expect` cannot work because the napi macro splits the fn"
)]
pub async fn route(
    endpoint: Option<String>,
    task: String,
    objective: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    context_tokens: Option<i64>,
    required_capabilities: Option<Vec<String>>,
    min_confidence: Option<String>,
    execution_preference: Option<String>,
    min_placement_evidence: Option<String>,
) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    // Forwarded unchanged so the router's confidence gate does the refusing for every caller.
    let body = json!({
        "task": task,
        "objective": objective.unwrap_or_else(|| "balanced".to_owned()),
        "model": model,
        "session_id": session_id,
        "context_tokens": context_tokens,
        "required_capabilities": required_capabilities.unwrap_or_default(),
        "min_confidence": min_confidence,
        "execution_preference": execution_preference.unwrap_or_else(|| "auto".to_owned()),
        "min_placement_evidence": min_placement_evidence.unwrap_or_else(|| "configured".to_owned()),
    });
    let value = post_json(
        &endpoint,
        "/_freellama/v1/routes",
        &body,
        crate::serve_client::control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Object-based managed task API for callers that need the complete task contract without a long,
/// ABI-fragile positional argument list. The request is forwarded to `/_freellama/v1/tasks`, where
/// the typed Rust server validates routing fields, message history, and advanced Ollama controls.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or rejects the request.
#[napi(ts_args_type = "endpoint: string | undefined | null, request: unknown")]
pub async fn run_task_request(endpoint: Option<String>, request: Value) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(
        &endpoint,
        "/_freellama/v1/tasks",
        &request,
        crate::serve_client::task_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Creates opt-in, bounded process-local conversation history.
/// # Errors
/// Returns an error if the server is unavailable or rejects the scope limits or messages.
#[napi(ts_args_type = "endpoint: string | undefined | null, request: unknown")]
pub async fn create_scope(endpoint: Option<String>, request: Value) -> Result<String> {
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            "/_freellama/v1/scopes",
            &request,
            crate::serve_client::control_timeout(),
        )
        .await?,
    )
}

/// Reads scope metadata; history is returned only when explicitly requested.
/// # Errors
/// Returns an error for an invalid ID or missing/expired scope.
#[napi]
pub async fn get_scope(
    endpoint: Option<String>,
    scope_id: String,
    include_messages: Option<bool>,
) -> Result<String> {
    let id = uuid::Uuid::parse_str(&scope_id).map_err(to_napi_err)?;
    pretty(
        &get_json(
            &endpoint_or_default(endpoint),
            &format!(
                "/_freellama/v1/scopes/{id}?include_messages={}",
                include_messages.unwrap_or(false)
            ),
            crate::serve_client::control_timeout(),
        )
        .await?,
    )
}

/// Copies a specified revision into an independent scope.
/// # Errors
/// Returns an error for an invalid ID, stale revision, or rejected limits.
#[napi(ts_args_type = "endpoint: string | undefined | null, scopeId: string, request: unknown")]
pub async fn fork_scope(
    endpoint: Option<String>,
    scope_id: String,
    request: Value,
) -> Result<String> {
    let id = uuid::Uuid::parse_str(&scope_id).map_err(to_napi_err)?;
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            &format!("/_freellama/v1/scopes/{id}/fork"),
            &request,
            crate::serve_client::control_timeout(),
        )
        .await?,
    )
}

/// Deletes history and invalidates in-flight commits for this scope.
/// # Errors
/// Returns an error for an invalid ID or unavailable server.
#[napi]
pub async fn delete_scope(endpoint: Option<String>, scope_id: String) -> Result<()> {
    let id = uuid::Uuid::parse_str(&scope_id).map_err(to_napi_err)?;
    delete_json(
        &endpoint_or_default(endpoint),
        &format!("/_freellama/v1/scopes/{id}"),
        crate::serve_client::control_timeout(),
    )
    .await
}

/// Preloads an exact installed model through managed admission and deadlines.
/// # Errors
/// Returns an error if the server refuses placement, capacity, or the request.
#[napi(ts_args_type = "endpoint: string | undefined | null, request: unknown")]
pub async fn warm_model_request(endpoint: Option<String>, request: Value) -> Result<String> {
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            "/_freellama/v1/warm",
            &request,
            crate::serve_client::task_timeout(),
        )
        .await?,
    )
}

/// Executes caller-declared independent managed tasks with bounded, priority-fair dispatch.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or rejects the batch envelope. Individual
/// task failures remain explicit entries in the successful batch response so sibling work is not
/// discarded.
#[napi(ts_args_type = "endpoint: string | undefined | null, request: unknown")]
pub async fn run_task_batch_request(endpoint: Option<String>, request: Value) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(
        &endpoint,
        "/_freellama/v1/task-batches",
        &request,
        crate::serve_client::task_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Lists bounded process-local deferred task receipts, without prompts or retained results.
///
/// # Errors
/// Returns an error if the managed server is unreachable or refuses the request.
#[napi]
pub async fn list_task_jobs(endpoint: Option<String>) -> Result<String> {
    pretty(
        &get_json(
            &endpoint_or_default(endpoint),
            "/_freellama/v1/jobs",
            crate::serve_client::control_timeout(),
        )
        .await?,
    )
}

/// Reads a deferred task receipt and its retained result, if completed.
///
/// # Errors
/// Returns an error for an invalid ID, an expired job, or an unavailable server.
#[napi]
pub async fn get_task_job(endpoint: Option<String>, job_id: String) -> Result<String> {
    let id =
        uuid::Uuid::parse_str(&job_id).map_err(|error| Error::from_reason(error.to_string()))?;
    pretty(
        &get_json(
            &endpoint_or_default(endpoint),
            &format!("/_freellama/v1/jobs/{id}"),
            crate::serve_client::control_timeout(),
        )
        .await?,
    )
}

/// Cancels one deferred task and waits for its local admission permits to be released.
///
/// # Errors
/// Returns an error for an invalid ID, an expired job, or an unavailable server.
#[napi]
pub async fn cancel_task_job(endpoint: Option<String>, job_id: String) -> Result<String> {
    let id =
        uuid::Uuid::parse_str(&job_id).map_err(|error| Error::from_reason(error.to_string()))?;
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            &format!("/_freellama/v1/jobs/{id}/cancel"),
            &Value::Null,
            crate::serve_client::task_timeout(),
        )
        .await?,
    )
}

/// Stops one deferred task, waits for local permits, and removes its retained record.
///
/// # Errors
/// Returns an error for an invalid ID, a missing job, or an unavailable server.
#[napi]
pub async fn remove_task_job(endpoint: Option<String>, job_id: String) -> Result<String> {
    let id = uuid::Uuid::parse_str(&job_id).map_err(to_napi_err)?;
    let endpoint = endpoint_or_default(endpoint);
    pretty(
        &request_json(
            crate::serve_client::client().delete(crate::ollama_http::url(
                &endpoint,
                &format!("/_freellama/v1/jobs/{id}"),
            )),
            crate::serve_client::task_timeout(),
        )
        .await?,
    )
}
