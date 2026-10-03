//! Node-native bindings (via `napi-rs`) exposing `FreeLlama`'s local-LLM control plane as plain
//! async functions, so a Node/TypeScript MCP server can define tools that call straight into this
//! binary instead of shelling out to the CLI or hand-rolling HTTP requests.
//!
//! This module is the *only* place in the crate allowed to contain `unsafe` (see the
//! `#[allow(unsafe_code)]` on its `pub mod napi;` declaration in `lib.rs`) — `napi-derive`'s
//! generated FFI glue requires it. Every exported function here is a thin async wrapper: it either
//! calls a plain library function directly (`doctor`) or makes one HTTP call to an already-running
//! `freellama serve` instance, exactly mirroring how `packages/cli/src/main.rs`'s CLI subcommands work
//! (`print_get`/`print_post`/`request_route` etc.) — this crate does not duplicate routing,
//! recommendation, or model-discovery logic a second time; the running server stays the single
//! source of truth for all of it.
//!
//! `doctor` is the one exception: it's a standalone library function (`crate::doctor`) that talks
//! directly to Ollama and needs no running `freellama serve` at all.

use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use reqwest::Client;
use serde_json::{Value, json};

/// Default for functions that need a running `freellama serve` (proxy + control plane).
/// Overridable via `FREELLAMA_SERVE_ENDPOINT` so a non-default port/host doesn't need a recompile.
const DEFAULT_SERVE_ENDPOINT: &str = "http://127.0.0.1:11435";
/// Default for `doctor`, which talks to Ollama directly and needs no `freellama serve` at all —
/// kept distinct from `DEFAULT_SERVE_ENDPOINT` so its documented "no serve required" behavior
/// actually matches its default (previously both shared the proxy port, which only worked by
/// accident because the proxy transparently forwards `/api/version`/`/api/ps`). Overridable via
/// `FREELLAMA_OLLAMA_ENDPOINT` — the same env var name the benchmark adapters already use.
const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

/// Timeout for the decision-only control-plane calls (machine/models/routes/recommendations).
/// These are pure computation on an in-memory model list; anything past a few seconds means the
/// server is wedged, not busy. Overridable via `FREELLAMA_CONTROL_TIMEOUT_SECONDS`.
fn control_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}

/// Timeout for managed inference and model loading. A cold load of a large model can
/// legitimately take minutes — Ollama's own `OLLAMA_LOAD_TIMEOUT` is 5m before it even gives up on
/// the load — so this has to be generous or it would abort work that was going to succeed.
/// Overridable via `FREELLAMA_TASK_TIMEOUT_SECONDS`.
fn task_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_TASK_TIMEOUT_SECONDS",
        crate::DEFAULT_TASK_TIMEOUT_SECS,
    )
}

fn endpoint_or_default(endpoint: Option<String>) -> String {
    endpoint
        .or_else(|| std::env::var("FREELLAMA_SERVE_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_SERVE_ENDPOINT.to_owned())
}

fn ollama_endpoint_or_default(endpoint: Option<String>) -> String {
    endpoint
        .or_else(|| std::env::var("FREELLAMA_OLLAMA_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_OLLAMA_ENDPOINT.to_owned())
}

fn to_napi_err(error: impl std::fmt::Display) -> Error {
    Error::from_reason(error.to_string())
}

/// One process-wide HTTP client, built on first use.
///
/// Note that `reqwest::Client` has **no request timeout by default** — only connect-refused fails
/// fast. Against an endpoint that accepts the TCP connection and then never answers, every tool
/// here used to hang forever (verified: `machine` against a black-hole listener was still pending
/// at 45s), contradicting this crate's own documented promise that these calls "return a clear
/// connection error, they won't hang". Timeouts are therefore applied per request rather than on
/// the shared client, because the control-plane calls and the generation calls need very
/// different ones.
///
/// `reqwest::Client` owns a connection pool, a DNS resolver, and background driver tasks; building
/// a fresh one per call (which this module used to do) throws all of that away on every tool
/// invocation and reconnects from scratch each time. The server side already holds a single client
/// in `PlatformState` — this makes the NAPI side match. Cloning is cheap and is the documented way
/// to share one: the clone is a handle onto the same pool, not a second pool.
fn client() -> Client {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(Client::new).clone()
}

fn authenticated(request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
    let Some(path) = std::env::var_os("FREELLAMA_AUTH_TOKEN_FILE") else {
        return Ok(request);
    };
    let token = std::fs::read_to_string(&path).map_err(to_napi_err)?;
    let token = token.trim();
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        return Err(Error::from_reason(
            "FREELLAMA_AUTH_TOKEN_FILE must contain one token of at least 32 bytes",
        ));
    }
    Ok(request.bearer_auth(token))
}

async fn get_json(endpoint: &str, path: &str, timeout: Duration) -> Result<Value> {
    request_json(
        client().get(format!("{}{path}", endpoint.trim_end_matches('/'))),
        timeout,
    )
    .await
}

async fn post_json(endpoint: &str, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
    request_json(
        client()
            .post(format!("{}{path}", endpoint.trim_end_matches('/')))
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
        client().delete(format!("{}{path}", endpoint.trim_end_matches('/'))),
        timeout,
    )
    .await?;
    Ok(())
}

fn pretty(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(to_napi_err)
}

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
    let value = get_json(&endpoint, "/_freellama/v1/machine", control_timeout()).await?;
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
    let value = get_json(&endpoint, "/_freellama/v1/health", control_timeout()).await?;
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
    let value = get_json(&endpoint, "/_freellama/v1/status", control_timeout()).await?;
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
    let value = get_json(&endpoint, &path, control_timeout()).await?;
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
        control_timeout(),
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
        control_timeout(),
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
        control_timeout(),
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
    let value = get_json(&endpoint, "/_freellama/v1/models", control_timeout()).await?;
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
#[allow(clippy::too_many_arguments)]
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
    // Forwarded so the CORE gate does the refusing. The MCP layer used to gate client-side with
    // its own rank map, where an unknown grade defaulted to rank 1 and silently passed — the same
    // fail-open bug the core gate was built to close. One gate, in the router, for every caller.
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
    let value = post_json(&endpoint, "/_freellama/v1/routes", &body, control_timeout()).await?;
    pretty(&value)
}

/// Side-effect-free install recommendation for a task, via `POST /_freellama/v1/recommendations`.
/// Never runs `ollama pull` itself — only proposes a plan.
///
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or rejects the request.
#[napi]
// This is a stable JavaScript FFI boundary: keeping the optional routing controls as separate
// arguments preserves the generated TypeScript API and matches `route`/`run_task`. Internal Rust
// callers use typed request structs, so the usual maintainability concern does not apply here.
#[allow(clippy::too_many_arguments)]
pub async fn recommend(
    endpoint: Option<String>,
    task: String,
    objective: Option<String>,
    model: Option<String>,
    context_tokens: Option<i64>,
    required_capabilities: Option<Vec<String>>,
    execution_preference: Option<String>,
    min_placement_evidence: Option<String>,
) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let body = json!({
        "task": task,
        "objective": objective.unwrap_or_else(|| "balanced".to_owned()),
        "model": model,
        "session_id": Value::Null,
        "context_tokens": context_tokens,
        "required_capabilities": required_capabilities.unwrap_or_default(),
        "execution_preference": execution_preference.unwrap_or_else(|| "auto".to_owned()),
        "min_placement_evidence": min_placement_evidence.unwrap_or_else(|| "configured".to_owned()),
    });
    let value = post_json(
        &endpoint,
        "/_freellama/v1/recommendations",
        &body,
        control_timeout(),
    )
    .await?;
    pretty(&value)
}

/// Routes AND executes a chat/generate/embed call in one shot, via `POST /_freellama/v1/tasks` on
/// a running `freellama serve`. Unlike `route`/`recommend` (which only ever return a decision,
/// never do work), this is `FreeLlama`'s actual "run something smart" entry point: it picks a model
/// exactly like `route` does, then immediately forwards the call to Ollama with the resulting
/// options (context window, thinking mode, `keep_alive`, etc.) applied.
///
/// Provide `prompt` for a single-turn message, or `messages` (a JSON array of
/// `{"role":...,"content":...}` objects) for multi-turn history — `messages` wins if both are set.
/// For a vision task, attach `images` (base64-encoded strings, no data-URI prefix) alongside
/// `prompt` — pair with `required_capabilities: ["vision"]` to ensure a vision-capable model gets
/// picked. For `task: "embedding"`, set `input` instead (a string or array of strings). `tools` is
/// an optional JSON array of tool/function definitions for function-calling tasks. `keep_alive`
/// overrides Ollama's default model residency window (e.g. `"0"` to unload immediately after this
/// call, `"-1"` for infinite) — omit it to keep the server's own default.
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or rejects the request
/// (e.g. an unknown task/objective, no eligible model, or neither `prompt`/`messages`/`input`
/// provided for a task that requires one).
#[napi]
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    endpoint: Option<String>,
    task: String,
    objective: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    context_tokens: Option<i64>,
    required_capabilities: Option<Vec<String>>,
    prompt: Option<String>,
    images: Option<Vec<String>>,
    messages: Option<Value>,
    input: Option<Value>,
    tools: Option<Value>,
    keep_alive: Option<String>,
    min_confidence: Option<String>,
    execution_preference: Option<String>,
    min_placement_evidence: Option<String>,
) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let body = json!({
        "min_confidence": min_confidence,
        "task": task,
        "objective": objective.unwrap_or_else(|| "balanced".to_owned()),
        "model": model,
        "session_id": session_id,
        "context_tokens": context_tokens,
        "required_capabilities": required_capabilities.unwrap_or_default(),
        "prompt": prompt,
        "images": images,
        "messages": messages.unwrap_or_else(|| Value::Array(Vec::new())),
        "input": input,
        "tools": tools,
        "keep_alive": keep_alive,
        "execution_preference": execution_preference.unwrap_or_else(|| "auto".to_owned()),
        "min_placement_evidence": min_placement_evidence.unwrap_or_else(|| "configured".to_owned()),
    });
    let value = post_json(&endpoint, "/_freellama/v1/tasks", &body, task_timeout()).await?;
    pretty(&value)
}

/// Object-based managed task API for callers that need the complete task contract without a long,
/// ABI-fragile positional argument list. The request is forwarded to `/_freellama/v1/tasks`, where
/// the typed Rust server validates routing fields, message history, and advanced Ollama controls.
///
/// # Errors
///
/// Returns an error if `freellama serve` is unreachable or rejects the request.
#[napi]
pub async fn run_task_request(endpoint: Option<String>, request: Value) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(&endpoint, "/_freellama/v1/tasks", &request, task_timeout()).await?;
    pretty(&value)
}

/// Creates opt-in, bounded process-local conversation history.
/// # Errors
/// Returns an error if the server is unavailable or rejects the scope limits or messages.
#[napi]
pub async fn create_scope(endpoint: Option<String>, request: Value) -> Result<String> {
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            "/_freellama/v1/scopes",
            &request,
            control_timeout(),
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
            control_timeout(),
        )
        .await?,
    )
}

/// Copies a specified revision into an independent scope.
/// # Errors
/// Returns an error for an invalid ID, stale revision, or rejected limits.
#[napi]
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
            control_timeout(),
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
        control_timeout(),
    )
    .await
}

/// Preloads an exact installed model through managed admission and deadlines.
/// # Errors
/// Returns an error if the server refuses placement, capacity, or the request.
#[napi]
pub async fn warm_model_request(endpoint: Option<String>, request: Value) -> Result<String> {
    pretty(
        &post_json(
            &endpoint_or_default(endpoint),
            "/_freellama/v1/warm",
            &request,
            task_timeout(),
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
#[napi]
pub async fn run_task_batch_request(endpoint: Option<String>, request: Value) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let value = post_json(
        &endpoint,
        "/_freellama/v1/task-batches",
        &request,
        task_timeout(),
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
            control_timeout(),
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
            control_timeout(),
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
            task_timeout(),
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
            client().delete(format!(
                "{}/_freellama/v1/jobs/{id}",
                endpoint.trim_end_matches('/')
            )),
            task_timeout(),
        )
        .await?,
    )
}

/// Converts a free-text natural-language intent into a route, via
/// `POST /_freellama/v1/natural-routes`.
///
/// # Errors
///
/// Returns an error if `freellama serve` isn't reachable at `endpoint`, or rejects the request.
#[napi]
pub async fn natural_route(
    endpoint: Option<String>,
    text: String,
    session_id: Option<String>,
) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let mut body = json!({ "text": text });
    if let Some(session_id) = session_id {
        body["session_id"] = Value::String(session_id);
    }
    let value = post_json(
        &endpoint,
        "/_freellama/v1/natural-routes",
        &body,
        task_timeout(),
    )
    .await?;
    pretty(&value)
}
