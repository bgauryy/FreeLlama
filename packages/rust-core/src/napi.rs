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

/// Default for the 5 functions that need a running `freellama serve` (proxy + control plane).
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
/// Timeout for the two calls that make a model actually generate. A cold load of a large model can
/// legitimately take minutes — Ollama's own `OLLAMA_LOAD_TIMEOUT` is 5m before it even gives up on
/// the load — so this has to be generous or it would abort work that was going to succeed.
/// Overridable via `FREELLAMA_TASK_TIMEOUT_SECONDS`.

fn control_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}

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

async fn get_json(endpoint: &str, path: &str, timeout: Duration) -> Result<Value> {
    client()
        .get(format!("{}{path}", endpoint.trim_end_matches('/')))
        .timeout(timeout)
        .send()
        .await
        .map_err(to_napi_err)?
        .error_for_status()
        .map_err(to_napi_err)?
        .json::<Value>()
        .await
        .map_err(to_napi_err)
}

async fn post_json(endpoint: &str, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
    let response = client()
        .post(format!("{}{path}", endpoint.trim_end_matches('/')))
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(to_napi_err)?;
    // `error_for_status()` discards the body — and the body is where every useful refusal lives.
    // A `min_confidence` refusal names the grade, the evidence, the model it would have picked and
    // the two commands that raise the grade; all of that was collapsing into a bare
    // "HTTP status client error (422)". Same defect, same fix as the CLI's `print_response`.
    let status = response.status();
    let value = response.json::<Value>().await.map_err(to_napi_err)?;
    if !status.is_success() {
        let detail = value
            .get("error")
            .and_then(Value::as_str)
            .map_or_else(|| value.to_string(), ToOwned::to_owned);
        return Err(napi::Error::from_reason(detail));
    }
    Ok(value)
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

/// Machine profile (chip, unified memory, CPU count, disk) as seen by a running `freellama serve`.
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
pub async fn route(
    endpoint: Option<String>,
    task: String,
    objective: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    context_tokens: Option<i64>,
    required_capabilities: Option<Vec<String>>,
    min_confidence: Option<String>,
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
pub async fn recommend(
    endpoint: Option<String>,
    task: String,
    objective: Option<String>,
    model: Option<String>,
    context_tokens: Option<i64>,
    required_capabilities: Option<Vec<String>>,
) -> Result<String> {
    let endpoint = endpoint_or_default(endpoint);
    let body = json!({
        "task": task,
        "objective": objective.unwrap_or_else(|| "balanced".to_owned()),
        "model": model,
        "session_id": Value::Null,
        "context_tokens": context_tokens,
        "required_capabilities": required_capabilities.unwrap_or_default(),
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
///
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
    });
    let value = post_json(&endpoint, "/_freellama/v1/tasks", &body, task_timeout()).await?;
    pretty(&value)
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
