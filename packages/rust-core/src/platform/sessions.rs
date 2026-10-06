//! Session lifecycle handlers and the cancellation guard for session-bound work.

use anyhow::Result;
use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use super::PlatformState;
use super::error::ApiError;

/// A session is only an affinity handle. Touch it before any expensive discovery or inference so
/// expired/deleted handles fail promptly and cannot be used to grow an unbounded in-memory map.
pub(crate) async fn require_active_session(
    state: &PlatformState,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if state
        .sessions
        .write()
        .await
        .touch(session_id, state.session_ttl)
    {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ))
    }
}

/// One cancellation boundary for every session-scoped model operation, including interpretation.
pub(crate) async fn with_session_cancellation<T>(
    state: &PlatformState,
    session_id: Option<&str>,
    work: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    let cancellation = if let Some(id) = session_id {
        // Register under the same lock as validation: kill cannot slip between them.
        let mut sessions = state.sessions.write().await;
        if !sessions.touch(id, state.session_ttl) {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "session does not exist or has expired",
            ));
        }
        sessions.cancellation(id)
    } else {
        None
    };
    if let Some(mut cancellation) = cancellation {
        tokio::select! {
            biased;
            () = async {
                if cancellation.wait_for(|killed| *killed).await.is_err() {
                    // Ordinary delete/idle expiry releases affinity only, retaining its contract.
                    std::future::pending::<()>().await;
                }
            } => Err(ApiError::session_killed()),
            result = work => result,
        }
    } else {
        work.await
    }
}

pub(crate) async fn create_session(
    State(state): State<PlatformState>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if sessions.len() >= state.max_sessions {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("session limit reached ({})", state.max_sessions),
        ));
    }
    let id = sessions.create();
    Ok(Json(json!({
        "session_id": id,
        "affinity": "model",
        "stores_prompt_or_kv": false,
        "idle_ttl_seconds": state.session_ttl.as_secs(),
    })))
}

pub(crate) async fn delete_session(
    State(state): State<PlatformState>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if sessions.remove(&session_id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ))
    }
}

pub(crate) async fn kill_session(
    State(state): State<PlatformState>,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.write().await;
    sessions.prune_expired(state.session_ttl);
    if !sessions.kill(&session_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session does not exist or has expired",
        ));
    }
    Ok(Json(json!({
        "session_id": session_id,
        "killed": true,
        "cancellation": "requested",
        "runner_stop_confirmed": false,
        "model_unloaded": false,
    })))
}
