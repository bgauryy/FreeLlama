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
    /// Part of the task already ran (an earlier embedding slice) before this error, so a
    /// fallback chain must not treat it as a refusal before execution.
    after_execution: bool,
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
    /// Seconds a caller should wait before retrying; also sent as the `Retry-After` header.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    /// When a load waited for memory: the processes holding the most of it, so the caller can
    /// ask its user what to close.
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_holders: Option<Vec<resources::MemoryHolder>>,
    /// The caller's `max_wait_seconds` when the operator's queue-wait cap lowered it.
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_limit: Option<Value>,
    /// With `fallback_models`: every model tried before this refusal and why each was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<Value>,
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
        /// The footprint estimate, when sizing finished before the deadline passed.
        #[serde(skip_serializing_if = "Option::is_none")]
        required_available_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        held_by: Vec<resources::PressureReason>,
    },
}

impl ApiError {
    pub(super) fn invalid_task_request(error: &axum::extract::rejection::JsonRejection) -> Self {
        Self::new(error.status(), error.body_text()).with_code("invalid_task_request")
    }

    pub(super) fn new(status: StatusCode, error: impl std::fmt::Display) -> Self {
        Self {
            status,
            body: Box::new(ErrorBody {
                error: error.to_string(),
                code: None,
                resource_admission: None,
                upstream_response: None,
                lifecycle: None,
                retry_after_seconds: None,
                memory_holders: None,
                wait_limit: None,
                fallback: None,
            }),
            after_execution: false,
        }
    }

    /// Mark an error that followed work already done for this task (see `after_execution`).
    pub(super) fn after_partial_execution(mut self) -> Self {
        self.after_execution = true;
        self
    }

    /// Whether part of the task ran before this error.
    pub(super) fn followed_execution(&self) -> bool {
        self.after_execution
    }

    /// A host-memory admission wait that ran out of time (as opposed to a queue or input error).
    pub(super) fn is_resource_wait(&self) -> bool {
        self.body.code == Some("resource_admission_unavailable")
    }

    /// A refusal because a wait (queue slot, model lifecycle or host resources) ran out of time.
    pub(super) fn is_wait_deadline(&self) -> bool {
        matches!(
            self.body.code,
            Some("admission_timeout" | "resource_admission_unavailable" | "model_lifecycle_busy")
        )
    }

    /// Name the wait the caller asked for and the operator cap that shortened it.
    pub(super) fn with_wait_limit(mut self, limit: Value) -> Self {
        if let Some(note) = limit.get("note").and_then(Value::as_str) {
            self.body.error = format!("{} ({note})", self.body.error);
        }
        self.body.wait_limit = Some(limit);
        self
    }

    pub(super) fn is_opportunistic_warm_skip(&self) -> bool {
        self.body.code == Some("opportunistic_warm_skipped")
    }

    /// Attach a machine-readable code.
    pub(super) fn with_code(mut self, code: &'static str) -> Self {
        self.body.code = Some(code);
        self
    }

    /// Tell the caller when capacity is expected back (`Retry-After`).
    pub(super) fn with_retry_after(mut self, seconds: u64) -> Self {
        self.body.retry_after_seconds = Some(seconds.max(1));
        self
    }

    pub(super) fn status(&self) -> StatusCode {
        self.status
    }

    pub(super) fn bad_request(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, error)
    }

    /// A routing refusal, keeping the stable code of a refused pinned model (not installed, or
    /// unable to do the task) so callers and fallback chains need not parse the message.
    pub(super) fn route_refusal(error: &anyhow::Error) -> Self {
        let refusal = error.downcast_ref::<super::ModelRefusal>();
        let api = Self::bad_request(error);
        match refusal {
            Some(refusal) => api.with_code(refusal.code),
            None => api,
        }
    }

    /// The machine-readable code, when one was attached.
    pub(super) fn code(&self) -> Option<&'static str> {
        self.body.code
    }

    /// The refusal message.
    pub(super) fn message(&self) -> &str {
        &self.body.error
    }

    /// The models an explicit fallback chain tried before this error, in order.
    pub(super) fn with_fallback(mut self, fallback: Value) -> Self {
        self.body.fallback = Some(fallback);
        self
    }

    pub(super) fn task_cancelled() -> Self {
        Self::new(StatusCode::CONFLICT, "task cancelled").with_code("task_cancelled")
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

    pub(super) fn with_lifecycle(mut self, lifecycle: Value) -> Self {
        self.body.lifecycle = Some(lifecycle);
        self
    }

    /// `required_available_bytes` is the sizing result when sizing finished: without it the
    /// deadline passed before the model could be sized, and the message says so.
    pub(super) fn with_resource_deadline(
        mut self,
        phase: &'static str,
        waited_ms: u128,
        required_available_bytes: Option<u64>,
    ) -> Self {
        let required_available_bytes = required_available_bytes.filter(|bytes| *bytes > 0);
        self.body.code = Some("resource_admission_unavailable");
        self.body.retry_after_seconds.get_or_insert(5);
        let words = match (phase, required_available_bytes) {
            ("initial_reservation", None) => {
                "timed out waiting for memory before the model could be sized"
            }
            ("initial_reservation", Some(_)) => {
                "timed out waiting for host memory and pressure to allow the model's memory reservation"
            }
            ("reservation_revalidation", _) => {
                "timed out re-checking host memory after the reservation was granted"
            }
            _ => "timed out waiting for host memory",
        };
        self.body.error = format!("{} ({words})", self.body.error);
        self.body.resource_admission = Some(ResourceFailureReceipt::Deadline {
            status: "deadline_exceeded",
            phase,
            waited_ms,
            required_available_bytes,
            held_by: Vec::new(),
        });
        self
    }

    /// Name the host signals that were holding work when a resource deadline passed, so a
    /// caller can tell heat from memory from CPU load. Who holds the memory is added by
    /// [`Self::with_memory_guidance`].
    pub(super) fn with_holds(mut self, reasons: Vec<resources::PressureReason>) -> Self {
        if let Some(ResourceFailureReceipt::Deadline { held_by, .. }) =
            self.body.resource_admission.as_mut()
        {
            if !reasons.is_empty() {
                let names = reasons
                    .iter()
                    .filter_map(|reason| serde_json::to_value(reason).ok())
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.body.error = format!("{}: host held by {names}", self.body.error);
            }
            *held_by = reasons;
        }
        self
    }

    /// The memory figures of an assessed refusal: what the load needs with its reserve, the
    /// reserve, and what is free; `None` for a deadline that has no assessment.
    pub(super) fn assessed_memory(&self) -> Option<MemoryFigures> {
        let Some(ResourceFailureReceipt::Assessed(receipt)) = &self.body.resource_admission else {
            return None;
        };
        let assessment = receipt.assessment.as_ref()?;
        Some(MemoryFigures {
            required_with_reserve: assessment.required_with_reserve_bytes.unwrap_or_else(|| {
                assessment
                    .required_available_bytes
                    .saturating_add(assessment.reserve_bytes)
            }),
            reserve: assessment.reserve_bytes,
            free: assessment.effective_available_bytes,
        })
    }

    /// The sizing result a deadline carried, if sizing finished before it.
    pub(super) fn deadline_required_bytes(&self) -> Option<u64> {
        match &self.body.resource_admission {
            Some(ResourceFailureReceipt::Deadline {
                required_available_bytes,
                ..
            }) => *required_available_bytes,
            _ => None,
        }
    }

    /// Whether a memory signal (not CPU load or heat) held the host when this refusal was made.
    pub(super) fn memory_held(&self) -> bool {
        match &self.body.resource_admission {
            Some(ResourceFailureReceipt::Assessed(receipt)) => {
                receipt.assessment.as_ref().is_some_and(|assessment| {
                    assessment.denial_reason
                        == Some(resources::CapacityDenialReason::InsufficientCapacity)
                        || assessment.held_by.iter().any(|reason| reason.is_memory())
                })
            }
            Some(ResourceFailureReceipt::Deadline { held_by, .. }) => {
                held_by.iter().any(|reason| reason.is_memory())
            }
            None => false,
        }
    }

    /// Add what an agent needs to act on a memory refusal: the figures (for a deadline whose
    /// sizing finished), who holds the memory, and which installed models would fit.
    pub(super) fn with_memory_guidance(
        mut self,
        figures: Option<MemoryFigures>,
        holders: Vec<resources::MemoryHolder>,
        alternatives: Option<String>,
    ) -> Self {
        if self.assessed_memory().is_none()
            && let Some(figures) = figures
        {
            self.body.error = format!("{}; {}", self.body.error, figures.sentence());
        }
        if !holders.is_empty() && self.body.memory_holders.is_none() {
            self.body.error = format!(
                "{}; largest memory users: {}",
                self.body.error,
                resources::describe_holders(&holders)
            );
            self.body.memory_holders = Some(holders);
        }
        if let Some(alternatives) = alternatives {
            self.body.error = format!("{}; {alternatives}", self.body.error);
        }
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
        let retry_after = self.body.retry_after_seconds;
        let mut response = (self.status, Json(self.body)).into_response();
        if let Some(seconds) = retry_after
            && let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

/// What a load needs against what the host has, in one definition: `required_with_reserve` is the
/// footprint plus the reserve the host keeps free, so it is the number to compare with `free`.
#[derive(Debug, Clone, Copy)]
pub(super) struct MemoryFigures {
    pub(super) required_with_reserve: u64,
    pub(super) reserve: u64,
    pub(super) free: Option<u64>,
}

impl MemoryFigures {
    /// "needs 15.6 GB incl. 2.0 GB reserve, 4.2 GB free".
    fn sentence(self) -> String {
        let free = self.free.map_or_else(
            || "free memory unknown".to_owned(),
            |free| format!("{} free", resources::gb(free)),
        );
        format!(
            "needs {} incl. {} reserve, {free}",
            resources::gb(self.required_with_reserve),
            resources::gb(self.reserve),
        )
    }
}

pub(super) fn resource_error(error: resources::ResourceWaitError) -> ApiError {
    let mut message = error.to_string();
    let receipt = *error.receipt;
    if let Some(assessment) = &receipt.assessment {
        let held = assessment
            .held_by
            .iter()
            .filter_map(|reason| serde_json::to_value(reason).ok())
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        if !held.is_empty() {
            message = format!("{message} (host held by {})", held.join(", "));
        }
        let figures = MemoryFigures {
            required_with_reserve: assessment.required_with_reserve_bytes.unwrap_or_else(|| {
                assessment
                    .required_available_bytes
                    .saturating_add(assessment.reserve_bytes)
            }),
            reserve: assessment.reserve_bytes,
            free: assessment.effective_available_bytes,
        };
        message = if assessment.required_available_bytes > 0 {
            format!("{message}; {}", figures.sentence())
        } else {
            let free = figures.free.map_or_else(
                || "free memory unknown".to_owned(),
                |free| format!("{} free", resources::gb(free)),
            );
            format!(
                "{message}; {free}, {} reserve",
                resources::gb(figures.reserve)
            )
        };
    }
    let memory_short = receipt.assessment.as_ref().is_some_and(|assessment| {
        assessment.denial_reason == Some(resources::CapacityDenialReason::InsufficientCapacity)
            || assessment.held_by.iter().any(|reason| reason.is_memory())
    });
    // Absent, never an empty list: a caller reads `[]` as "nothing holds the memory".
    let memory_holders = memory_short
        .then(resources::memory_holders)
        .filter(|holders| !holders.is_empty());
    if let Some(holders) = &memory_holders {
        message = format!(
            "{message}; largest memory users: {}",
            resources::describe_holders(holders)
        );
    }
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        body: Box::new(ErrorBody {
            error: message,
            code: Some("resource_admission_unavailable"),
            resource_admission: Some(ResourceFailureReceipt::Assessed(Box::new(receipt))),
            upstream_response: None,
            lifecycle: None,
            // Host memory rarely recovers in under a few seconds.
            retry_after_seconds: Some(5),
            memory_holders,
            wait_limit: None,
            fallback: None,
        }),
        after_execution: false,
    }
}

#[cfg(test)]
mod structured_error_tests {
    use super::{ApiError, resource_error, resources};
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn partial_resource_deadlines_include_retry_guidance() {
        let response = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "resource deadline")
            .with_resource_deadline("initial_reservation", 1001, None)
            .into_response();
        assert_eq!(response.headers()["retry-after"], "5");
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(payload["retry_after_seconds"], 5);
        assert_eq!(payload["code"], "resource_admission_unavailable");
        assert_eq!(
            payload["resource_admission"]["phase"],
            "initial_reservation"
        );
        let custom = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "resource deadline")
            .with_retry_after(11)
            .with_resource_deadline("reservation_revalidation", 20, None)
            .into_batch_result("custom".into());
        assert_eq!(custom["retry_after_seconds"], 11);
    }

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
        .with_resource_deadline("initial_reservation", 12, None)
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
            receipt: Box::new(resources::ResourceReceipt {
                status: "held",
                waited_ms: 12,
                required_available_bytes: 4096,
                reserved_bytes: 128,
                snapshot: None,
                assessment: None,
            }),
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
            receipt: Box::new(resources::ResourceReceipt {
                status: "held",
                waited_ms: 12,
                required_available_bytes: 4096,
                reserved_bytes: 0,
                snapshot: None,
                assessment: None,
            }),
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

    const GIB: u64 = 1024 * 1024 * 1024;

    fn assessed(
        required: u64,
        reserve: u64,
        free: Option<u64>,
        held_by: Vec<resources::PressureReason>,
    ) -> resources::ResourceWaitError {
        resources::ResourceWaitError {
            receipt: Box::new(resources::ResourceReceipt {
                status: "held",
                waited_ms: 1,
                required_available_bytes: required,
                reserved_bytes: 0,
                snapshot: None,
                assessment: Some(resources::ResourceCapacityAssessment {
                    status: "held_insufficient_capacity",
                    admissible: false,
                    denial_reason: Some(resources::CapacityDenialReason::InsufficientCapacity),
                    memory_advisory: None,
                    held_by,
                    required_available_bytes: required,
                    reserve_bytes: reserve,
                    required_with_reserve_bytes: Some(required + reserve),
                    effective_available_bytes: free,
                    missing_telemetry: Vec::new(),
                }),
            }),
        }
    }

    #[test]
    fn refusal_text_states_needs_reserve_and_free_with_the_receipt_numbers() {
        // 13.5 GB footprint + 2 GB reserve: the text must say 15.5 (the receipt's
        // required_with_reserve_bytes), never the bare footprint.
        let result = resource_error(assessed(
            27 * GIB / 2,
            2 * GIB,
            Some(4 * GIB),
            vec![resources::PressureReason::LowAvailableMemory],
        ))
        .into_batch_result("a".into());
        let text = result["error"].as_str().unwrap();
        assert!(
            text.contains("needs 15.5 GB incl. 2.0 GB reserve, 4.0 GB free"),
            "{text}"
        );
        assert!(text.contains("host held by low_available_memory"), "{text}");
        let fields = &result["resource_admission"]["assessment"];
        assert_eq!(fields["required_with_reserve_bytes"], 31 * GIB / 2);
        assert_eq!(fields["reserve_bytes"], 2 * GIB);
        assert_eq!(fields["effective_available_bytes"], 4 * GIB);
        assert!(result.get("memory_holders").is_none_or(|holders| {
            holders
                .as_array()
                .is_some_and(|holders| !holders.is_empty())
        }));
    }

    #[test]
    fn refusal_text_says_when_free_memory_is_unknown() {
        let result =
            resource_error(assessed(4 * GIB, GIB, None, Vec::new())).into_batch_result("a".into());
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("needs 5.0 GB incl. 1.0 GB reserve, free memory unknown")
        );
    }

    #[test]
    fn deadline_before_sizing_names_the_phase_and_keeps_no_byte_claim() {
        let error = ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "resource admission deadline exceeded",
        )
        .with_resource_deadline("initial_reservation", 5, None)
        .with_holds(vec![resources::PressureReason::LowAvailableMemory]);
        assert!(error.memory_held());
        assert_eq!(error.deadline_required_bytes(), None);
        let result = error.into_batch_result("a".into());
        let text = result["error"].as_str().unwrap();
        assert!(
            text.contains("timed out waiting for memory before the model could be sized"),
            "{text}"
        );
        assert!(text.contains("host held by low_available_memory"), "{text}");
    }

    #[test]
    fn memory_guidance_adds_figures_holders_and_alternatives_to_a_deadline() {
        let holder = resources::MemoryHolder {
            name: "node: tsserver.js".into(),
            processes: 2,
            pid: 7,
            resident_bytes: 5 * GIB,
        };
        let error = ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "resource admission deadline exceeded",
        )
        .with_resource_deadline("initial_reservation", 5, Some(14 * GIB))
        .with_holds(vec![resources::PressureReason::LowAvailableMemory]);
        assert_eq!(error.deadline_required_bytes(), Some(14 * GIB));
        let figures = super::MemoryFigures {
            required_with_reserve: 16 * GIB,
            reserve: 2 * GIB,
            free: Some(4 * GIB),
        };
        let result = error
            .with_memory_guidance(
                Some(figures),
                vec![holder],
                Some("installed `decision` models that fit".into()),
            )
            .into_batch_result("a".into());
        let text = result["error"].as_str().unwrap();
        assert!(
            text.contains("needs 16.0 GB incl. 2.0 GB reserve, 4.0 GB free"),
            "{text}"
        );
        assert!(
            text.contains("largest memory users: node: tsserver.js ×2 5.0 GB"),
            "{text}"
        );
        assert!(
            text.ends_with("installed `decision` models that fit"),
            "{text}"
        );
        assert_eq!(result["memory_holders"][0]["pid"], 7);
        assert_eq!(
            result["resource_admission"]["required_available_bytes"],
            14 * GIB
        );
    }

    #[test]
    fn a_cpu_load_hold_is_not_a_memory_refusal() {
        let error = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "deadline")
            .with_resource_deadline("initial_reservation", 5, None)
            .with_holds(vec![resources::PressureReason::HighCpuLoad]);
        assert!(!error.memory_held());
    }
}
