//! Upstream transport for managed tasks: one POST to Ollama with the passthrough proxy's retry
//! schedule, as a single JSON body or as a streamed `/api/chat` that can stop a repetition loop.

use axum::http::StatusCode;
use serde_json::{Value, json};
use std::time::Instant;

use super::super::error::ApiError;
use super::super::{PlatformState, repetition};
use crate::proxy;

/// Retry 500/502 (load-model blips), but not 503 busy or 504 timeout. Same as passthrough.
/// Retrying 503 while holding an admission slot — and on a cold load, the exclusive write lock —
/// amplifies the saturation the semaphore exists to shed.
fn retryable_managed_status(status: StatusCode) -> bool {
    proxy::retryable_upstream_status(status)
}

/// Send `body` to `url`, retrying transient failures on the passthrough proxy's backoff
/// schedule (`proxy::retry_delay`), and return the first response that is final.
///
/// The caller holds the `managed_execution` admission permit across this call: an Ollama 500 —
/// which it returns under load-model contention, the exact condition managed routing creates —
/// must not fail the task and throw away a slot it already queued for.
async fn send_with_retries(
    state: &PlatformState,
    url: &str,
    path: &str,
    body: &Value,
) -> Result<reqwest::Response, ApiError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let more_attempts = attempt < proxy::MAX_ATTEMPTS;
        match state.client.post(url).json(body).send().await {
            Ok(response) if retryable_managed_status(response.status()) && more_attempts => {
                eprintln!(
                    "managed task retry attempt={attempt} status={} path={path}",
                    response.status()
                );
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Ok(response) => return Ok(response),
            // A timeout is NOT a transient hiccup here. This client's per-attempt budget is
            // `platform_task_timeout()` (900s by default), and the caller holds both an admission
            // slot and — on the non-resident path — the exclusive `managed_execution` write lock
            // across every attempt. Retrying a timeout would therefore hold the whole managed
            // plane for up to 3 x 900s, which is exactly the "one hung request deadlocks every
            // subsequent managed task" failure the client timeout was added to prevent. Connection
            // errors are still retried: those fail fast and cost nothing to re-pay.
            Err(error) if more_attempts && error.is_connect() && !error.is_timeout() => {
                eprintln!("managed task retry attempt={attempt} error={error:#} path={path}");
                tokio::time::sleep(proxy::retry_delay(attempt)).await;
            }
            Err(error) => return Err(ApiError::upstream(error)),
        }
    }
}

/// A whole response body as JSON. A failing Ollama does not always answer in JSON (a wedged
/// runner can return a plain-text or HTML body); a strict parse would turn a truthful 500 into a
/// misleading decode error, so a non-JSON body is carried through as text.
async fn body_value(response: reqwest::Response) -> Result<(StatusCode, Value), ApiError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(ApiError::upstream)?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes).trim().to_owned() }));
    Ok((status, value))
}

/// POST JSON upstream with retries and return the status and body.
pub(crate) async fn post_json_with_retries(
    state: &PlatformState,
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let url = crate::ollama_http::url(upstream, path);
    body_value(send_with_retries(state, &url, path, body).await?).await
}

/// The response a stopped stream reports: everything generated so far, `done:true` with the
/// stop reason, the streamed token count, and the sidecar's own wall time. Ollama's timing fields
/// arrive only in its final chunk, so they are absent and the sample trains no throughput.
fn streamed_completion(
    last: &Value,
    message: &Value,
    tokens: u64,
    started: Instant,
    reason: &str,
) -> Value {
    json!({
        "model": last["model"],
        "created_at": last["created_at"],
        "message": message,
        "done": true,
        "done_reason": reason,
        "eval_count": tokens,
        "total_duration": u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
    })
}

/// The accumulated assistant message of a stream.
#[derive(Default)]
struct StreamedMessage {
    content: String,
    thinking: String,
    tool_calls: Vec<Value>,
    tokens: u64,
}

impl StreamedMessage {
    fn absorb(&mut self, chunk: &Value) {
        if let Some(piece) = chunk.pointer("/message/content").and_then(Value::as_str)
            && !piece.is_empty()
        {
            self.content.push_str(piece);
            self.tokens += 1;
        }
        if let Some(piece) = chunk.pointer("/message/thinking").and_then(Value::as_str) {
            self.thinking.push_str(piece);
        }
        if let Some(calls) = chunk
            .pointer("/message/tool_calls")
            .and_then(Value::as_array)
        {
            self.tool_calls.extend(calls.iter().cloned());
        }
    }

    fn message(&self) -> Value {
        let mut message = json!({"role": "assistant", "content": self.content});
        if !self.thinking.is_empty() {
            message["thinking"] = json!(self.thinking);
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = json!(self.tool_calls);
        }
        message
    }
}

/// How often, in streamed tokens, a growing output is checked for a loop.
const LOOP_CHECK_TOKENS: u64 = 16;

/// [`post_json_with_retries`] for a streaming `/api/chat` (`stop_on_repetition`): the same retries
/// before the first byte, then Ollama's NDJSON chunks are accumulated into one non-streaming
/// response. Every [`LOOP_CHECK_TOKENS`] tokens the text is checked for a repetition loop; on one,
/// the response is dropped, which closes the connection and cancels the generation upstream, and
/// everything generated so far is returned with `done_reason: "repetition"`. Ollama's own
/// "token repeat limit" abort keeps the generated text too (`done_reason: "repeat_limit"`).
pub(crate) async fn post_streaming_chat_with_retries(
    state: &PlatformState,
    upstream: &str,
    path: &str,
    body: &Value,
) -> Result<(StatusCode, Value), ApiError> {
    let started = Instant::now();
    let url = crate::ollama_http::url(upstream, path);
    let mut response = send_with_retries(state, &url, path, body).await?;
    let status = response.status();
    if !status.is_success() {
        return body_value(response).await;
    }
    let mut pending = Vec::new();
    let mut streamed = StreamedMessage::default();
    let mut last = Value::Null;
    let mut checked_at = 0u64;
    while let Some(bytes) = response.chunk().await.map_err(ApiError::upstream)? {
        pending.extend_from_slice(&bytes);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=end).collect::<Vec<_>>();
            let Ok(chunk) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
                if error
                    .as_str()
                    .is_some_and(|text| text.contains("repeat limit"))
                    && !streamed.content.is_empty()
                {
                    let value = streamed_completion(
                        &last,
                        &streamed.message(),
                        streamed.tokens,
                        started,
                        "repeat_limit",
                    );
                    return Ok((StatusCode::OK, value));
                }
                // Any other upstream error keeps the text generated before it.
                let mut value = chunk.clone();
                value["message"] = streamed.message();
                return Ok((status, value));
            }
            streamed.absorb(&chunk);
            if chunk["done"] == true {
                let mut value = chunk;
                value["message"] = streamed.message();
                return Ok((status, value));
            }
            last = chunk;
            if streamed.tokens >= checked_at + LOOP_CHECK_TOKENS {
                checked_at = streamed.tokens;
                if repetition::should_stop_streaming(&streamed.content) {
                    // Returning drops `response`: the connection closes and Ollama cancels.
                    let value = streamed_completion(
                        &last,
                        &streamed.message(),
                        streamed.tokens,
                        started,
                        "repetition",
                    );
                    return Ok((StatusCode::OK, value));
                }
            }
        }
    }
    // The stream ended without a final chunk: report it as incomplete, keeping the text.
    Ok((
        status,
        json!({"done": false, "message": streamed.message()}),
    ))
}
