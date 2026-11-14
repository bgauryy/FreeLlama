//! Byte-preserving forwarding to Ollama, hop-by-hop header filtering and JSON refusal shapes.

use std::time::Instant;

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode},
};

use super::retry::{is_connection_refused, send_with_retries, try_restart_ollama};
use super::{ProxyState, body_budget, proxy_target};

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// A typed refusal: this generation's allocation could not be forecast, so it is not admitted.
pub(super) fn model_forecast_unavailable(reason: &str) -> Response<Body> {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({"error": reason, "code":"model_forecast_unavailable", "retry_after_seconds":1})
            .to_string(),
    );
    response
        .headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    response
}

pub(super) fn json_response(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("static response is valid")
}

/// Whether reading a body failed on the length limit (`to_bytes` wraps the limit's
/// [`http_body_util::LengthLimitError`]) rather than on a broken or aborted upload.
pub(super) fn exceeds_length_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|error| {
        error
            .downcast_ref::<http_body_util::LengthLimitError>()
            .is_some()
    })
}

pub(super) fn too_large(limit: usize) -> Response<Body> {
    json_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        serde_json::json!({"error": format!(
            "request body is over {limit} bytes; raise FREELLAMA_MAX_REQUEST_BYTES to accept it"
        )})
        .to_string(),
    )
}

/// Too many request bodies are buffered at once; this one was never read.
pub(super) fn body_budget_exhausted(error: body_budget::BodyBudgetExhausted) -> Response<Body> {
    let mut response = json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({"error": error.to_string(), "code": "request_body_budget_exhausted", "retry_after_seconds": 1})
            .to_string(),
    );
    response
        .headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    response
}

pub(super) fn refusal(
    status: StatusCode,
    message: &str,
    retry_after_seconds: u64,
) -> Response<Body> {
    let mut response = json_response(
        status,
        serde_json::json!({"error": message, "retry_after_seconds": retry_after_seconds})
            .to_string(),
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after_seconds.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

pub(super) async fn forward_inner(
    state: &ProxyState,
    parts: &axum::http::request::Parts,
    body_bytes: &Bytes,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let target = proxy_target(&state.upstream, parts.uri.to_string().as_str())?;
    let headers = filtered_headers(&parts.headers);
    let outcome = send_with_retries(state, &parts.method, &target, &headers, body_bytes).await;
    let response = match outcome {
        Ok(response) => response,
        Err(error)
            if state.auto_restart_ollama
                && is_connection_refused(&error)
                && try_restart_ollama(state).await =>
        {
            // One more full attempt (with its own internal MAX_ATTEMPTS retries) after the
            // restart — not an unbounded loop back into this same branch.
            send_with_retries(state, &parts.method, &target, &headers, body_bytes).await?
        }
        Err(error) => return Err(error),
    };
    let status = response.status();
    let headers = filtered_headers(response.headers());
    let mut outgoing = Response::builder().status(status);
    for (name, value) in &headers {
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.header("x-freellama-proxy", "1");
    let result = outgoing
        .body(Body::from_stream(response.bytes_stream()))
        .context("build proxied response")?;
    eprintln!(
        "proxy method={} path={} upstream_status={} upstream_headers_ms={}",
        parts.method,
        parts.uri,
        status,
        started.elapsed().as_millis()
    );
    Ok(result)
}

pub(super) fn filtered_headers(input: &HeaderMap) -> HeaderMap {
    let mut output = HeaderMap::new();
    for (name, value) in input {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            // Clone, don't round-trip through bytes. The input is a `HeaderMap`, so every name and
            // value here was already validated when it was parsed; re-parsing them only added two
            // `expect()` calls — the sole panic sites in the proxy — to re-derive what the type
            // system already guarantees, on every proxied request in both directions.
            output.append(name.clone(), value.clone());
        }
    }
    output
}
