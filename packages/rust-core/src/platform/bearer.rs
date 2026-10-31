//! Bearer-token authentication for the platform HTTP API.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Clone)]
pub(crate) struct AuthState {
    /// The operator token: every route, no budget.
    pub(crate) token: Option<Arc<str>>,
    pub(crate) clients: Arc<[super::clients::ClientKey]>,
    pub(crate) telemetry: super::Telemetry,
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub(crate) async fn require_bearer(
    State(auth): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::as_bytes);
    let matches =
        |token: &str| supplied.is_some_and(|value| constant_time_eq(value, token.as_bytes()));
    if auth.token.as_deref().is_some_and(matches) {
        return next.run(request).await;
    }
    // Compare against every key, so the time taken does not tell which one is close.
    let client = auth.clients.iter().fold(None, |found, key| {
        if matches(&key.token) {
            Some(key)
        } else {
            found
        }
    });
    let Some(client) = client else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": "missing or invalid bearer token"})),
        )
            .into_response();
    };
    let (method, path) = (request.method().clone(), request.uri().path().to_owned());
    if !client.admin && super::clients::needs_admin(&method, &path) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": format!("client key {} may not change models or reload the runtime file", client.name),
                "code": "client_not_admin",
            })),
        )
            .into_response();
    }
    if let Some(budget) = client.daily_tokens
        && super::clients::spends_tokens(&method, &path)
    {
        let used = auth.telemetry.client_tokens_today(&client.name);
        if used >= budget {
            let retry_after =
                super::clients::seconds_until_next_day(super::telemetry::now_seconds());
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after.to_string())],
                Json(json!({
                    "error": format!("client key {} has used its {budget} tokens for today (UTC)", client.name),
                    "code": "client_budget_exhausted",
                    "used_tokens": used,
                    "daily_tokens": budget,
                    "retry_after_seconds": retry_after,
                })),
            )
                .into_response();
        }
    }
    request
        .extensions_mut()
        .insert(crate::proxy::Caller(Arc::from(client.name.as_str())));
    next.run(request).await
}
