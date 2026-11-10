//! The raw proxy handler: body budget and buffering, admission order, locks and response wrap.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use http_body::Body as _;

use super::admission::AdmittedBody;
use super::lifecycle::{expects_terminal, forward_owned_unload, raw_transition_watch, usage_tap};
use super::raw_request::{RawModel, RawRequestKind, classify, raw_resource_permit};
use super::transport::{
    body_budget_exhausted, exceeds_length_limit, forward_inner, json_response,
    model_forecast_unavailable, refusal, too_large,
};
use super::upstream_forward::route_to_upstream;
use super::{ProxyState, RawAdmission, expected_body_bytes, systemone};

const RAW_FORECAST_WAIT: Duration = Duration::from_secs(10);

#[expect(
    clippy::too_many_lines,
    reason = "admission steps must stay in this order"
)]
pub(super) async fn forward(
    State(state): State<ProxyState>,
    request: Request,
) -> impl IntoResponse {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let declared = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|bytes| bytes > state.max_request_bytes) {
        return too_large(state.max_request_bytes);
    }
    // Reserve the body's size before reading it, so concurrent uploads cannot each hold the full
    // request limit in memory. A body without a declared length reserves the limit. The
    // reservation lives as long as the buffered bytes: until this handler returns.
    let expected =
        declared.unwrap_or_else(|| expected_body_bytes(&body.size_hint(), state.max_request_bytes));
    let budget_wait = state
        .admission
        .as_ref()
        .map_or(state.raw_queue_wait, RawAdmission::wait);
    let _body_reservation = match state.body_budget.reserve(expected, budget_wait).await {
        Ok(reservation) => reservation,
        Err(exhausted) => return body_budget_exhausted(exhausted),
    };
    // Buffer the body before taking any lock: a retried attempt must resend the exact same bytes,
    // and a slow or incomplete upload must never hold the shared execution lock while it trickles.
    let body_bytes = match tokio::time::timeout(
        state.request_body_timeout,
        // Buffered (not streamed) so a failed attempt can be resent byte-for-byte.
        to_bytes(body, state.max_request_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            eprintln!("proxy error: buffer request body: {error:#}");
            // A chunked upload has no length up front; its overflow surfaces here.
            return if exceeds_length_limit(&error) {
                too_large(state.max_request_bytes)
            } else {
                json_response(
                    StatusCode::BAD_REQUEST,
                    r#"{"error":"could not read request body"}"#.into(),
                )
            };
        }
        Err(_) => {
            return json_response(
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"request body read deadline exceeded"}"#.into(),
            );
        }
    };
    let body_bytes = if parts.method == reqwest::Method::POST && parts.uri.path() == "/v1/systemone"
    {
        match systemone::assets_as_images(&body_bytes) {
            Ok(Some(translated)) => translated,
            Ok(None) => body_bytes,
            Err(message) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error": message}).to_string(),
                );
            }
        }
    } else {
        body_bytes
    };
    if let Some(response) = route_to_upstream(&state, &parts, &body_bytes, started).await {
        return response;
    }
    let kind = classify(&parts.method, parts.uri.path(), &body_bytes);
    let kind_label = match kind {
        RawRequestKind::Metadata => "metadata",
        RawRequestKind::ModelStore => "model_store",
        RawRequestKind::Unload => "unload",
        RawRequestKind::Generation => "generation",
    };
    let count = |outcome: &str| {
        if let Some(telemetry) = &state.telemetry {
            telemetry.record_raw(kind_label, outcome);
        }
    };
    let generation = kind == RawRequestKind::Generation;
    let mut model = matches!(kind, RawRequestKind::Generation | RawRequestKind::Unload)
        .then(|| RawModel::parse(&body_bytes))
        .flatten();
    let wait = state
        .admission
        .as_ref()
        .map_or(state.raw_queue_wait, RawAdmission::wait);
    let deadline = tokio::time::Instant::now() + wait;
    // Register synchronously before the bounded raw queue, protecting queued demand. Wait
    // for an existing unload only after a slot is held, so there is no extra unbounded queue.
    let mut in_use = state
        .hooks
        .as_ref()
        .zip(model.as_ref().filter(|_| generation))
        .map(|(hooks, model)| hooks.begin(&model.name));
    // Slot first, then the execution lock, so a queued raw request never holds the lock while
    // waiting for a slot.
    let admission = if kind == RawRequestKind::Unload {
        Some(&state.unload_admission)
    } else {
        state.admission.as_ref().filter(|_| generation)
    };
    let permit = match admission {
        Some(admission) => match admission.acquire(deadline).await {
            Ok(permit) => Some(permit),
            Err(rejection) => {
                count("rejected_limit");
                return refusal(
                    StatusCode::TOO_MANY_REQUESTS,
                    &format!(
                        "proxy busy: {}; use managed /_freellama/v1/tasks for weighted admission",
                        rejection.reason
                    ),
                    rejection.retry_after_seconds,
                );
            }
        },
        None => None,
    };
    if let Some((hooks, model)) = state.hooks.as_ref().zip(model.as_ref()) {
        let ready = if generation {
            tokio::time::timeout_at(deadline, hooks.ready(&model.name))
                .await
                .is_ok()
        } else if let Ok(lease) =
            tokio::time::timeout_at(deadline, hooks.acquire_unload(&model.name)).await
        {
            in_use = Some(lease);
            true
        } else {
            false
        };
        if !ready {
            count("rejected_busy");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "model lifecycle is busy; retry after its current work finishes",
                1,
            );
        }
    }
    if kind == RawRequestKind::Unload && model.is_some() && in_use.is_some() {
        // Once dispatched, an explicit unload keeps its lifecycle lease even if the caller
        // disconnects. Its small response is buffered unchanged; cleanup has a fixed bound.
        let cleanup = tokio::spawn(forward_owned_unload(
            state.clone(),
            parts,
            body_bytes,
            model,
            permit,
            in_use,
        ));
        return cleanup.await.unwrap_or_else(|error| json_response(StatusCode::BAD_GATEWAY,
            serde_json::json!({"error":format!("unload supervisor failed: {error}"),"code":"unload_outcome_unknown"}).to_string()));
    }
    if generation
        && let Some(model) = model.as_mut()
        && tokio::time::timeout(RAW_FORECAST_WAIT, model.hydrate(&state))
            .await
            .is_err()
    {
        return model_forecast_unavailable("model metadata forecast timed out");
    }
    // A generation whose model cannot be resolved may still load one (a multipart body Ollama
    // reads but this proxy does not), and nothing could size or serialize that load. Refuse it
    // rather than admit it with a zero-byte forecast. A well-formed JSON body that names no
    // usable model is different: Ollama rejects it before scheduling, so it can load nothing.
    if generation
        && model.is_none()
        && serde_json::from_slice::<serde_json::Value>(&body_bytes).is_err()
    {
        count("rejected_pressure");
        return model_forecast_unavailable(
            "request model could not be resolved, so its allocation cannot be forecast",
        );
    }
    // A raw generation on a runner Ollama already holds at a compatible identity takes no
    // backend lock: its model use lease keeps the runner from being evicted while it decodes,
    // and a cold load of another model does not wait for it. Only a load owns the load lock,
    // until its runner is observed ready. Residency is observed again after the lock is won, so
    // a request that queued behind its own model's load shares the loaded runner.
    let mut observed = None;
    let mut load = if generation
        && model.is_some()
        && let Some(lock) = state.load_lock.as_ref()
    {
        let resident_now = match (&state.hooks, &mut model) {
            (Some(_), Some(model)) => Some(model.observe(&state).await),
            _ => None,
        };
        if resident_now == Some(true) {
            observed = Some(true);
            None
        } else if let Ok(guard) =
            tokio::time::timeout_at(deadline, Arc::clone(lock).lock_owned()).await
        {
            let resident_now = match (&state.hooks, &mut model) {
                (Some(_), Some(model)) => Some(model.observe(&state).await),
                _ => None,
            };
            observed = resident_now;
            (resident_now != Some(true)).then_some(guard)
        } else {
            count("rejected_busy");
            return refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy busy: another model load is active; use managed /_freellama/v1/tasks to queue",
                2,
            );
        }
    } else {
        None
    };
    // With platform hooks, residency decides whether this request loads a model: a load makes
    // room first, as a managed load would, and the model counts as busy until the response ends.
    let resident = match (&state.hooks, &mut model) {
        (Some(hooks), Some(model)) => {
            let resident = match observed {
                Some(resident) => resident,
                None => model.observe(&state).await,
            };
            if generation && !resident {
                hooks.prepare(&model.name, model.context).await;
            }
            Some(resident)
        }
        _ => None,
    };
    let mut resource = if generation {
        match tokio::time::timeout(
            RAW_FORECAST_WAIT,
            raw_resource_permit(&state, model.as_mut(), resident),
        )
        .await
        {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(error)) => {
                count("rejected_pressure");
                let message = if error.receipt.status == "model_forecast_unavailable" {
                    "local model allocation could not be forecast; retry when Ollama model metadata is available".to_owned()
                } else {
                    error.to_string()
                };
                let mut refusal = json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({"error": message, "code": error.receipt.status, "resource_admission": error.receipt})
                        .to_string(),
                );
                refusal
                    .headers_mut()
                    .insert("retry-after", axum::http::HeaderValue::from_static("2"));
                return refusal;
            }
            Err(_) => {
                count("rejected_pressure");
                return refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "raw resource forecast or admission exceeded its deadline",
                    1,
                );
            }
        }
    } else {
        None
    };
    let mut transition_watch = if load.is_some() {
        model
            .as_ref()
            .filter(|model| model.digest.is_some())
            .map(|model| raw_transition_watch(&state, model.clone()))
    } else {
        None
    };
    // Nonstream Ollama responses can withhold headers until decoding ends. Observe readiness
    // while forwarding too, so those requests release the load lock as soon as their runner is
    // ready; an unfinished watch follows a streaming response into its body below.
    let forwarded = forward_inner(&state, &parts, &body_bytes);
    tokio::pin!(forwarded);
    let response = if let Some(watch) = transition_watch.as_mut() {
        tokio::select! {
            response = &mut forwarded => response,
            () = watch => {
                transition_watch = None;
                load = None;
                if crate::platform::host_has_unified_memory()
                    && let Some(resource) = resource.as_mut()
                {
                    resource.release();
                }
                forwarded.await
            }
        }
    } else {
        forwarded.await
    };
    match response {
        Ok(response) => {
            count("forwarded");
            let mut usage = None;
            if response.status().is_success()
                && let (Some(hooks), Some(model)) = (&state.hooks, &model)
            {
                if generation {
                    let reuse_unverified = resident == Some(false);
                    usage = Some(usage_tap(
                        hooks,
                        "gpu",
                        &model.name,
                        &parts,
                        reuse_unverified,
                        started,
                        expects_terminal(parts.uri.path(), &body_bytes),
                    ));
                } else if resident == Some(true) {
                    // Only a runner that was loaded is unloaded; anything else changed nothing.
                    hooks.unload_accepted(&model.name);
                }
            }
            let (response_parts, body) = response.into_parts();
            Response::from_parts(
                response_parts,
                Body::new(AdmittedBody {
                    inner: body,
                    permit,
                    load,
                    resource,
                    in_use,
                    usage,
                    transition_watch,
                }),
            )
        }
        Err(error) => {
            count("upstream_error");
            drop((permit, load, resource));
            eprintln!("proxy error: {error:#}");
            json_response(
                StatusCode::BAD_GATEWAY,
                r#"{"error":"upstream unavailable"}"#.into(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    /// A full buffered-body budget refuses a body it would have to read, before reading it, and
    /// still forwards requests with no body.
    #[tokio::test]
    async fn a_full_body_budget_refuses_uploads_with_a_code_but_not_empty_requests() {
        use axum::{
            body::{Body, to_bytes},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;

        let upstream = axum::Router::new().fallback(|| async { "ok" });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let budget = crate::proxy::BodyBudget::new(4096);
        let app = crate::proxy::app(
            crate::proxy::ProxyConfig::new("127.0.0.1:0", format!("http://{address}"), false)
                .with_body_budget(budget.clone()),
        )
        .unwrap();
        let held = budget
            .reserve(4096, std::time::Duration::ZERO)
            .await
            .unwrap();
        let upload = || {
            Request::post("/api/show")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"m"}"#))
                .unwrap()
        };
        let refused = app.clone().oneshot(upload()).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.headers()["retry-after"], "1");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(refused.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["code"], "request_body_budget_exhausted");
        let empty = app
            .clone()
            .oneshot(Request::get("/api/tags").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            empty.status(),
            StatusCode::OK,
            "an empty body reserves nothing"
        );
        drop(held);
        let forwarded = app.oneshot(upload()).await.unwrap();
        assert_eq!(forwarded.status(), StatusCode::OK);
        assert_eq!(
            budget.receipt()["available_bytes"],
            4096,
            "returned with the body"
        );
        server.abort();
    }

    /// A body without a declared length (chunked) that passes the limit is refused with the same
    /// 413 as one whose `content-length` declares it; a body that fails for another reason is a
    /// 400. Detection follows the error's source chain to `LengthLimitError`, not its text.
    #[tokio::test]
    async fn an_oversized_chunked_body_is_refused_with_413_and_a_broken_one_with_400() {
        use axum::{
            body::{Body, Bytes, to_bytes},
            http::{Request, StatusCode},
        };
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        use tower::ServiceExt as _;

        /// Frames without a size hint, ending in `error` when one is given.
        struct Chunked {
            frames: Vec<Bytes>,
            error: Option<std::io::Error>,
        }
        impl http_body::Body for Chunked {
            type Data = Bytes;
            type Error = std::io::Error;
            fn poll_frame(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
                let this = self.get_mut();
                Poll::Ready(if this.frames.is_empty() {
                    this.error.take().map(Err)
                } else {
                    Some(Ok(http_body::Frame::data(this.frames.remove(0))))
                })
            }
        }
        let app = crate::proxy::app(
            crate::proxy::ProxyConfig::new("127.0.0.1:0", "http://127.0.0.1:9", false)
                .with_body_budget(crate::proxy::BodyBudget::new(4096))
                .with_max_request_bytes(16),
        )
        .unwrap();
        let send = |body: Body, declared: Option<usize>| {
            let mut request =
                Request::post("/api/generate").header("content-type", "application/json");
            if let Some(bytes) = declared {
                request = request.header("content-length", bytes);
            }
            app.clone().oneshot(request.body(body).unwrap())
        };
        let read = |response: axum::response::Response| async move {
            let status = response.status();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            (status, body)
        };
        let expected = serde_json::json!({
            "error": "request body is over 16 bytes; raise FREELLAMA_MAX_REQUEST_BYTES to accept it"
        });
        let chunked = Body::new(Chunked {
            frames: vec![
                Bytes::from_static(b"0123456789"),
                Bytes::from_static(b"0123456789"),
            ],
            error: None,
        });
        assert_eq!(
            read(send(chunked, None).await.unwrap()).await,
            (StatusCode::PAYLOAD_TOO_LARGE, expected.clone())
        );
        let declared = Body::from(vec![b'x'; 20]);
        assert_eq!(
            read(send(declared, Some(20)).await.unwrap()).await,
            (StatusCode::PAYLOAD_TOO_LARGE, expected)
        );
        let broken = Body::new(Chunked {
            frames: vec![Bytes::from_static(b"{")],
            error: Some(std::io::Error::other("length limit exceeded")),
        });
        assert_eq!(
            read(send(broken, None).await.unwrap()).await,
            (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": "could not read request body"})
            ),
            "an upload error that merely mentions the limit is not an oversized body"
        );
    }
}
