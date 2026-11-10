//! Routing OpenAI-compatible requests to the configured engine that serves the named model.

use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
};

use crate::platform::resources::ResourceDemand;

use super::admission::AdmittedBody;
use super::lifecycle::{expects_terminal, usage_tap};
use super::transport::{filtered_headers, json_response, refusal};
use super::{ProxyState, RAW_PRESSURE_WAIT, upstreams};

/// An OpenAI-compatible request for a model another engine serves, answered by that engine.
/// `None` sends the request on to Ollama as before.
pub(super) async fn route_to_upstream(
    state: &ProxyState,
    parts: &axum::http::request::Parts,
    body: &Bytes,
    started: Instant,
) -> Option<Response<Body>> {
    let upstreams = state.upstreams.as_ref()?;
    let path = parts.uri.path();
    if !path.starts_with("/v1/") {
        return None;
    }
    if parts.method == reqwest::Method::GET && path == "/v1/models" {
        let merged = upstreams
            .merged_models(&state.client, &state.upstream)
            .await;
        return Some(json_response(StatusCode::OK, merged.to_string()));
    }
    if parts.method != reqwest::Method::POST {
        return None;
    }
    let mut request = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let model = request.get("model")?.as_str()?.to_owned();
    let upstreams::Route::Upstream(index, name) = upstreams
        .route(&state.client, &state.upstream, &model)
        .await
    else {
        return None;
    };
    let body = if name == model {
        body.clone()
    } else {
        request["model"] = serde_json::json!(name);
        Bytes::from(request.to_string())
    };
    Some(forward_to_upstream(state, upstreams, index, parts, body, &model, started).await)
}

async fn forward_to_upstream(
    state: &ProxyState,
    upstreams: &upstreams::Upstreams,
    index: usize,
    parts: &axum::http::request::Parts,
    body: Bytes,
    model: &str,
    started: Instant,
) -> Response<Body> {
    let requires_terminal = expects_terminal(parts.uri.path(), &body);
    let upstream = &upstreams.list[index];
    // An engine on this machine shares its memory and heat: wait out a hold as a resident model
    // would. A remote one does not.
    let resource = if crate::net::is_loopback_url(&upstream.base_url) {
        match state
            .resource_governor
            .wait_for_demand(
                &upstream.base_url,
                ResourceDemand::resident(),
                RAW_PRESSURE_WAIT,
            )
            .await
        {
            Ok(permit) => Some(permit),
            Err(error) => return refusal(StatusCode::SERVICE_UNAVAILABLE, &error.to_string(), 2),
        }
    } else {
        None
    };
    let mut headers = filtered_headers(&parts.headers);
    // The caller's bearer token is for FreeLlama; the upstream gets its own key or none.
    headers.remove(axum::http::header::AUTHORIZATION);
    let mut outgoing = state
        .client
        .post(upstreams.url(index, &parts.uri.to_string()))
        .headers(headers)
        .body(body);
    if let Some(key) = &upstream.api_key {
        outgoing = outgoing.bearer_auth(key);
    }
    let count = |outcome: &str| {
        if let Some(telemetry) = &state.telemetry {
            telemetry.record_raw("upstream", outcome);
        }
    };
    let response = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            count("upstream_error");
            eprintln!("proxy error: upstream {}: {error:#}", upstream.name);
            return json_response(
                StatusCode::BAD_GATEWAY,
                serde_json::json!({"error": format!("upstream {} unavailable", upstream.name)})
                    .to_string(),
            );
        }
    };
    count("forwarded");
    let status = response.status();
    let mut builder = Response::builder().status(status);
    for (header, value) in &filtered_headers(response.headers()) {
        builder = builder.header(header, value);
    }
    let usage = state
        .hooks
        .as_ref()
        .filter(|_| status.is_success())
        .map(|hooks| {
            usage_tap(
                hooks,
                &upstream.name,
                model,
                parts,
                false,
                started,
                requires_terminal,
            )
        });
    builder
        .header("x-freellama-proxy", "1")
        .header("x-freellama-upstream", upstream.name.as_str())
        .body(Body::new(AdmittedBody {
            inner: Body::from_stream(response.bytes_stream()),
            permit: None,
            load: None,
            resource,
            in_use: None,
            usage,
            transition_watch: None,
        }))
        .unwrap_or_else(|_| {
            json_response(
                StatusCode::BAD_GATEWAY,
                r#"{"error":"upstream response headers are invalid"}"#.into(),
            )
        })
}
