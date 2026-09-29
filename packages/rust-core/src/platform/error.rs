//! Typed errors for managed platform endpoints.
use super::resources;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    pub(super) body: Box<ErrorBody>,
}

#[derive(Debug, Serialize)]
pub(super) struct ErrorBody {
    pub(super) error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_admission: Option<ResourceFailureReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle: Option<Value>,
}

/// A deadline can expire before footprint discovery establishes a byte requirement. Keep that
/// partial observation explicit rather than fabricating zero required bytes or losing the phase.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResourceFailureReceipt {
    Assessed(Box<resources::ResourceReceipt>),
    Deadline {
        status: &'static str,
        phase: &'static str,
        waited_ms: u128,
    },
}

impl ApiError {
    pub(super) fn new(status: StatusCode, error: impl std::fmt::Display) -> Self {
        Self {
            status,
            body: Box::new(ErrorBody {
                error: error.to_string(),
                code: None,
                resource_admission: None,
                upstream_response: None,
                lifecycle: None,
            }),
        }
    }

    /// A host-memory admission wait that ran out of time (as opposed to a queue or input error).
    pub(super) fn is_resource_wait(&self) -> bool {
        self.body.code == Some("resource_admission_unavailable")
    }

    pub(super) fn bad_request(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, error)
    }

    pub(super) fn session_killed() -> Self {
        let mut error = Self::new(
            StatusCode::CONFLICT,
            "session killed; managed request cancelled",
        );
        error.body.code = Some("session_killed");
        error
    }

    pub(super) fn upstream(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, error)
    }

    pub(super) fn with_upstream_response(
        mut self,
        code: &'static str,
        response: Value,
        lifecycle: Option<Value>,
    ) -> Self {
        self.body.code = Some(code);
        self.body.upstream_response = Some(response);
        self.body.lifecycle = lifecycle;
        self
    }

    pub(super) fn with_resource_deadline(mut self, phase: &'static str, waited_ms: u128) -> Self {
        self.body.code = Some("resource_admission_unavailable");
        self.body.resource_admission = Some(ResourceFailureReceipt::Deadline {
            status: "deadline_exceeded",
            phase,
            waited_ms,
        });
        self
    }

    pub(super) fn into_batch_result(self, id: String) -> Value {
        let mut result = json!(self.body);
        result["id"] = Value::String(id);
        result["ok"] = json!(false);
        result["status"] = json!(self.status.as_u16());
        result
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

pub(super) fn resource_error(error: resources::ResourceWaitError) -> ApiError {
    let message = error.to_string();
    let receipt = error.receipt;
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        body: Box::new(ErrorBody {
            error: message,
            code: Some("resource_admission_unavailable"),
            resource_admission: Some(ResourceFailureReceipt::Assessed(Box::new(receipt))),
            upstream_response: None,
            lifecycle: None,
        }),
    }
}

#[cfg(test)]
mod structured_error_tests {
    use super::{ApiError, resource_error, resources};
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn ordinary_errors_keep_the_existing_string_only_body() {
        let response = ApiError::bad_request("invalid task").into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(payload, json!({"error": "invalid task"}));
    }

    #[test]
    fn deadline_batch_result_keeps_the_partial_resource_receipt() {
        let result = ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "resource deadline exceeded",
        )
        .with_resource_deadline("initial_reservation", 12)
        .into_batch_result("task-a".into());
        assert_eq!(result["status"], 503);
        assert_eq!(result["id"], "task-a");
        assert_eq!(result["code"], "resource_admission_unavailable");
        assert_eq!(result["resource_admission"]["status"], "deadline_exceeded");
        assert_eq!(result["resource_admission"]["phase"], "initial_reservation");
        assert!(
            result["resource_admission"]
                .get("required_available_bytes")
                .is_none()
        );
    }

    #[test]
    fn assessed_batch_result_preserves_machine_readable_resource_fields() {
        let result = resource_error(resources::ResourceWaitError {
            receipt: resources::ResourceReceipt {
                status: "held",
                waited_ms: 12,
                required_available_bytes: 4096,
                reserved_bytes: 128,
                snapshot: None,
                assessment: None,
            },
        })
        .into_batch_result("task-b".into());
        assert_eq!(result["status"], 503);
        assert_eq!(result["ok"], false);
        assert_eq!(result["code"], "resource_admission_unavailable");
        assert_eq!(
            result["resource_admission"]["required_available_bytes"],
            4096
        );
        assert_eq!(result["resource_admission"]["reserved_bytes"], 128);
    }

    #[tokio::test]
    async fn resource_refusal_has_a_human_message_and_structured_receipt() {
        let response = resource_error(resources::ResourceWaitError {
            receipt: resources::ResourceReceipt {
                status: "held",
                waited_ms: 12,
                required_available_bytes: 4096,
                reserved_bytes: 0,
                snapshot: None,
                assessment: None,
            },
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(payload["resource_admission"].is_object());
        assert_eq!(
            payload["resource_admission"]["required_available_bytes"],
            4096
        );
        assert_eq!(payload["code"], "resource_admission_unavailable");
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .starts_with("host resource admission")
        );
    }
}
