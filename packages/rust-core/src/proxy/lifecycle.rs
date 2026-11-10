//! Usage telemetry taps, supervised explicit unloads, and the cold-load transition watch.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Response, StatusCode},
};

use super::raw_request::RawModel;
use super::transport::{forward_inner, json_response};
use super::{Caller, ProxyState, RawFinished, RawModelHooks, RawOutcome, admission, usage};

/// Read what a response reports using and hand it to the platform once it ends.
pub(super) fn usage_tap(
    hooks: &Arc<dyn RawModelHooks>,
    backend: &str,
    model: &str,
    parts: &axum::http::request::Parts,
    reuse_unverified: bool,
    started: Instant,
    requires_terminal: bool,
) -> admission::UsageTap {
    let hooks = Arc::clone(hooks);
    let (backend, model, path) = (
        backend.to_owned(),
        model.to_owned(),
        parts.uri.path().to_owned(),
    );
    let caller = parts
        .extensions
        .get::<Caller>()
        .map(|caller| Arc::clone(&caller.0));
    admission::UsageTap {
        scanner: usage::UsageScanner::default(),
        requires_terminal,
        finished: Some(Box::new(move |usage, outcome| {
            if outcome == RawOutcome::Complete && backend == "gpu" {
                hooks.served(&model, reuse_unverified);
            }
            hooks.finished(RawFinished {
                backend: &backend,
                model: &model,
                path: &path,
                caller: caller.as_deref(),
                reuse_unverified,
                usage,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                outcome,
            });
        })),
    }
}

pub(super) fn expects_terminal(path: &str, body: &[u8]) -> bool {
    let body: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    body["stream"]
        .as_bool()
        .unwrap_or(matches!(path, "/api/generate" | "/api/chat"))
}

pub(super) async fn forward_owned_unload(
    state: ProxyState,
    parts: axum::http::request::Parts,
    body: Bytes,
    model: Option<RawModel>,
    permit: Option<admission::RawPermit>,
    in_use: Option<Box<dyn Send>>,
) -> Response<Body> {
    let _leases = (permit, in_use);
    let forwarded = tokio::time::timeout(Duration::from_secs(5), async {
        let response = forward_inner(&state, &parts, &body).await?;
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, state.max_request_bytes).await?;
        Ok::<_, anyhow::Error>(Response::from_parts(parts, Body::from(bytes)))
    })
    .await;
    let response = match forwarded {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => json_response(StatusCode::BAD_GATEWAY, serde_json::json!({"error":error.to_string(),"code":"unload_outcome_unknown"}).to_string()),
        Err(_) => json_response(StatusCode::GATEWAY_TIMEOUT, serde_json::json!({"error":"unload request timed out; upstream outcome is unknown","code":"unload_outcome_unknown"}).to_string()),
    };
    if let Some((hooks, model)) = state.hooks.as_ref().zip(model.as_ref()) {
        if response.status().is_success() {
            hooks.unload_accepted(&model.name);
        }
        // Even a failed exchange may have delivered the unload. Keep ownership until absence
        // is confirmed or this bounded unknown-outcome window expires.
        let confirmation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(response) = state
                    .client
                    .get(format!("{}/api/ps", state.upstream.trim_end_matches('/')))
                    .send()
                    .await
                    && response.status().is_success()
                    && let Ok(ps) = response.json::<serde_json::Value>().await
                    && ps["models"].as_array().is_some_and(|entries| {
                        entries
                            .iter()
                            .all(|entry| crate::ollama_http::ps_entry_name(entry).is_some())
                    })
                    && crate::ollama_http::find_ps_entry(&ps, &model.name).is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        if confirmation.is_ok() {
            hooks.unload_confirmed(&model.name);
        }
    }
    state.resource_governor.invalidate().await;
    response
}

pub(super) fn raw_transition_watch(
    state: &ProxyState,
    model: RawModel,
) -> admission::TransitionWatch {
    let client = state.client.clone();
    let upstream = state.upstream.clone();
    let hooks = state.hooks.clone();
    Box::pin(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(response) = client
                .get(format!("{}/api/ps", upstream.trim_end_matches('/')))
                .send()
                .await
                && let Ok(ps) = response.json::<serde_json::Value>().await
                && model.transition_compatible(&ps)
            {
                if let Some(hooks) = &hooks {
                    hooks.runner_ready();
                }
                return;
            }
        }
    })
}
