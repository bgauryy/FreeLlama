//! Opt-in process-local histories. A revision lease owns each append until commit or drop.
use super::{ApiError, PlatformState, RouteInput, TaskInput, TaskKind, context};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ScopePolicy {
    pub max_count: usize,
    pub max_messages: usize,
    pub max_bytes: usize,
    pub max_estimated_tokens: u64,
    pub total_max_bytes: usize,
    pub ttl_seconds: u64,
}
impl Default for ScopePolicy {
    fn default() -> Self {
        Self {
            max_count: 128,
            max_messages: 256,
            max_bytes: 1_048_576,
            max_estimated_tokens: 32_768,
            total_max_bytes: 16_777_216,
            ttl_seconds: 3600,
        }
    }
}
impl ScopePolicy {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_count > 0
                && self.max_messages > 0
                && self.max_bytes > 0
                && self.max_estimated_tokens > 0
                && self.total_max_bytes > 0
                && self.ttl_seconds > 0,
            "scope limits must be positive"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScopeLimitsInput {
    max_messages: Option<usize>,
    max_bytes: Option<usize>,
    max_estimated_tokens: Option<u64>,
    ttl_seconds: Option<u64>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ScopeLimits {
    max_messages: usize,
    max_bytes: usize,
    max_estimated_tokens: u64,
    ttl_seconds: u64,
}
impl ScopeLimits {
    fn resolve(input: &ScopeLimitsInput, policy: &ScopePolicy) -> Result<Self, ApiError> {
        let result = Self {
            max_messages: input.max_messages.unwrap_or(policy.max_messages),
            max_bytes: input.max_bytes.unwrap_or(policy.max_bytes),
            max_estimated_tokens: input
                .max_estimated_tokens
                .unwrap_or(policy.max_estimated_tokens),
            ttl_seconds: input.ttl_seconds.unwrap_or(policy.ttl_seconds),
        };
        if result.max_messages == 0
            || result.max_messages > policy.max_messages
            || result.max_bytes == 0
            || result.max_bytes > policy.max_bytes
            || result.max_estimated_tokens == 0
            || result.max_estimated_tokens > policy.max_estimated_tokens
            || result.ttl_seconds == 0
            || result.ttl_seconds > policy.ttl_seconds
        {
            return Err(scope_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope_limit_invalid",
                "scope limits must be positive and within operator caps",
            ));
        }
        Ok(result)
    }
    fn bounded(&self, policy: &ScopePolicy) -> Self {
        Self {
            max_messages: self.max_messages.min(policy.max_messages),
            max_bytes: self.max_bytes.min(policy.max_bytes),
            max_estimated_tokens: self.max_estimated_tokens.min(policy.max_estimated_tokens),
            ttl_seconds: self.ttl_seconds.min(policy.ttl_seconds),
        }
    }
}

/// Optional fields preserve caller omission; defaults never override explicit task routing.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RouteDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<TaskKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    objective: Option<super::Objective>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_capabilities: Option<BTreeSet<crate::model_bench::Capability>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_preference: Option<super::ExecutionPreference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_placement_evidence: Option<super::PlacementEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_confidence: Option<String>,
}
impl RouteDefaults {
    fn apply(&self, input: &mut TaskInput) -> Result<(), ApiError> {
        let mut route = serde_json::to_value(&input.route).expect("route serializes");
        let defaults = serde_json::to_value(self).expect("defaults serialize");
        for (key, value) in defaults.as_object().expect("object") {
            if !input.route_fields.contains(key) {
                route[key].clone_from(value);
            }
        }
        input.route = serde_json::from_value::<RouteInput>(route).map_err(ApiError::bad_request)?;
        if matches!(input.route.task, TaskKind::Embedding) {
            return Err(scope_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope_embedding_unsupported",
                "embedding tasks cannot use history scopes",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateInput {
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    route_defaults: RouteDefaults,
    #[serde(default)]
    limits: ScopeLimitsInput,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ForkInput {
    revision: u64,
    route_defaults: Option<RouteDefaults>,
    limits: Option<ScopeLimitsInput>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct GetInput {
    include_messages: bool,
}

struct History {
    revision: u64,
    messages: Vec<Value>,
    defaults: RouteDefaults,
    limits: ScopeLimits,
    touched: Instant,
    expires_at: u64,
    lease: Option<String>,
    forked_from: Option<Value>,
}
impl History {
    fn receipt(&self, id: &str, include: bool) -> Value {
        let (bytes, tokens) = history_size(&self.messages);
        let mut result = json!({"scope_id":id,"revision":self.revision,"route_defaults":self.defaults,"limits":self.limits,"storage":"process_local","privacy":{"opt_in":true,"persistent":false,"contains_caller_history":true},"message_count":self.messages.len(),"bytes":bytes,"estimated_tokens":tokens,"token_estimator":"utf8_bytes_div_3","expires_at":self.expires_at});
        if include {
            result["messages"] = json!(self.messages);
        }
        if let Some(source) = &self.forked_from {
            result["forked_from"].clone_from(source);
        }
        result
    }
}
#[derive(Clone, Default)]
pub(super) struct ScopeStore(Arc<Mutex<BTreeMap<String, History>>>);
impl ScopeStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, History>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn prune(entries: &mut BTreeMap<String, History>, policy: &ScopePolicy) {
        entries.retain(|_, entry| {
            entry.touched.elapsed()
                < Duration::from_secs(entry.limits.ttl_seconds.min(policy.ttl_seconds))
        });
    }
    fn capacity(
        entries: &BTreeMap<String, History>,
        bytes: usize,
        policy: &ScopePolicy,
    ) -> Result<(), ApiError> {
        if entries.len() >= policy.max_count
            || total_bytes(entries).saturating_add(bytes) > policy.total_max_bytes
        {
            return Err(scope_error(
                StatusCode::TOO_MANY_REQUESTS,
                "scope_capacity_exceeded",
                "history store capacity exceeded; delete scopes or lower history size",
            ));
        }
        Ok(())
    }
    fn create(&self, input: CreateInput, policy: &ScopePolicy) -> Result<Value, ApiError> {
        validate_messages(&input.messages)?;
        if input.route_defaults.task == Some(TaskKind::Embedding) {
            return Err(scope_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope_embedding_unsupported",
                "embedding tasks cannot use history scopes",
            ));
        }
        let limits = ScopeLimits::resolve(&input.limits, policy)?;
        validate_size(&input.messages, &limits)?;
        let mut entries = self.lock();
        Self::prune(&mut entries, policy);
        Self::capacity(&entries, history_size(&input.messages).0, policy)?;
        let id = Uuid::new_v4().to_string();
        let history = History {
            revision: 0,
            messages: input.messages,
            defaults: input.route_defaults,
            expires_at: super::telemetry::now_seconds().saturating_add(limits.ttl_seconds),
            limits,
            touched: Instant::now(),
            lease: None,
            forked_from: None,
        };
        let receipt = history.receipt(&id, false);
        entries.insert(id, history);
        Ok(receipt)
    }
    fn get(&self, id: &str, include: bool, policy: &ScopePolicy) -> Result<Value, ApiError> {
        let mut entries = self.lock();
        Self::prune(&mut entries, policy);
        entries
            .get(id)
            .map(|history| history.receipt(id, include))
            .ok_or_else(not_found)
    }
    fn fork(&self, id: &str, input: ForkInput, policy: &ScopePolicy) -> Result<Value, ApiError> {
        let mut entries = self.lock();
        Self::prune(&mut entries, policy);
        let source = entries.get(id).ok_or_else(not_found)?;
        if source.revision != input.revision {
            return Err(revision_conflict());
        }
        let limits = if let Some(limits) = &input.limits {
            ScopeLimits::resolve(limits, policy)?
        } else {
            source.limits.bounded(policy)
        };
        validate_size(&source.messages, &limits)?;
        let defaults = input
            .route_defaults
            .unwrap_or_else(|| source.defaults.clone());
        if defaults.task == Some(TaskKind::Embedding) {
            return Err(scope_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope_embedding_unsupported",
                "embedding scopes are unsupported",
            ));
        }
        let history = History {
            revision: 0,
            messages: source.messages.clone(),
            defaults,
            expires_at: super::telemetry::now_seconds().saturating_add(limits.ttl_seconds),
            limits,
            touched: Instant::now(),
            lease: None,
            forked_from: Some(json!({"scope_id":id,"revision":input.revision})),
        };
        Self::capacity(&entries, history_size(&history.messages).0, policy)?;
        let new_id = Uuid::new_v4().to_string();
        let receipt = history.receipt(&new_id, false);
        entries.insert(new_id, history);
        Ok(receipt)
    }
    pub(super) fn prepare(
        &self,
        input: &mut TaskInput,
        policy: &ScopePolicy,
    ) -> Result<Option<ScopeLease>, ApiError> {
        let (id, revision) = match (&input.scope_id, input.scope_revision) {
            (None, None) => return Ok(None),
            (Some(id), Some(revision)) => (id.clone(), revision),
            _ => {
                return Err(scope_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "scope_reference_invalid",
                    "scope_id and scope_revision must be supplied together",
                ));
            }
        };
        if input.input.is_some() {
            return Err(scope_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope_embedding_unsupported",
                "embedding input cannot use history scopes",
            ));
        }
        let mut entries = self.lock();
        Self::prune(&mut entries, policy);
        let history = entries.get_mut(&id).ok_or_else(not_found)?;
        if history.revision != revision {
            return Err(revision_conflict());
        }
        if history.lease.is_some() {
            return Err(scope_error(
                StatusCode::CONFLICT,
                "scope_busy",
                "scope already has an in-flight append; fork its snapshot for parallel work",
            ));
        }
        history.defaults.apply(input)?;
        let mut additions = std::mem::take(&mut input.messages);
        if additions.is_empty() {
            let prompt = input.prompt.take().ok_or_else(|| {
                ApiError::bad_request("scope task requires new prompt or messages")
            })?;
            let mut message = json!({"role":"user","content":prompt});
            if let Some(images) = input.images.take() {
                message["images"] = json!(images);
            }
            additions.push(message);
        }
        validate_messages(&additions)?;
        let mut messages = history.messages.clone();
        messages.extend(additions);
        let limits = history.limits.bounded(policy);
        validate_size(&messages, &limits)?;
        let lease_id = Uuid::new_v4().to_string();
        history.lease = Some(lease_id.clone());
        input.messages.clone_from(&messages);
        input.prompt = None;
        input.images = None;
        Ok(Some(ScopeLease {
            store: self.clone(),
            id,
            revision,
            lease_id,
            messages,
            result_budget: input
                .job_progress
                .as_ref()
                .map_or(usize::MAX, |_| super::jobs::JobProgress::result_budget()),
        }))
    }
}
/// Dropping a cancelled or failed task releases its exclusive append without changing history.
pub(super) struct ScopeLease {
    store: ScopeStore,
    id: String,
    revision: u64,
    lease_id: String,
    messages: Vec<Value>,
    result_budget: usize,
}
impl ScopeLease {
    pub(super) fn commit(
        mut self,
        result: &mut Value,
        policy: &ScopePolicy,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), ApiError> {
        let response = &result["response"];
        if response["done"] != true {
            return Err(scope_error(
                StatusCode::BAD_GATEWAY,
                "scope_response_invalid",
                "scope append requires a completed upstream response",
            ));
        }
        let assistant = response["message"].clone();
        validate_messages(std::slice::from_ref(&assistant)).map_err(|_| {
            scope_error(
                StatusCode::BAD_GATEWAY,
                "scope_response_invalid",
                "upstream assistant message is invalid",
            )
        })?;
        if assistant["role"] != "assistant" {
            return Err(scope_error(
                StatusCode::BAD_GATEWAY,
                "scope_response_invalid",
                "upstream response must have role assistant",
            ));
        }
        self.messages.push(assistant);
        let mut entries = self.store.lock();
        ScopeStore::prune(&mut entries, policy);
        let history = entries.get(&self.id).ok_or_else(not_found)?;
        if history.revision != self.revision || history.lease.as_deref() != Some(&self.lease_id) {
            return Err(revision_conflict());
        }
        let limits = history.limits.bounded(policy);
        validate_size(&self.messages, &limits)?;
        let projected = total_bytes(&entries)
            .saturating_sub(history_size(&history.messages).0)
            .saturating_add(history_size(&self.messages).0);
        if projected > policy.total_max_bytes {
            return Err(scope_error(
                StatusCode::TOO_MANY_REQUESTS,
                "scope_capacity_exceeded",
                "history store byte capacity exceeded; response was not appended",
            ));
        }
        let history = entries.get(&self.id).expect("validated history exists");
        let candidate = History {
            revision: history.revision + 1,
            messages: std::mem::take(&mut self.messages),
            defaults: history.defaults.clone(),
            limits: history.limits.clone(),
            touched: Instant::now(),
            expires_at: super::telemetry::now_seconds().saturating_add(limits.ttl_seconds),
            lease: None,
            forked_from: history.forked_from.clone(),
        };
        let mut receipt = candidate.receipt(&self.id, false);
        receipt["previous_revision"] = json!(self.revision);
        receipt["committed"] = json!(true);
        result["scope"] = receipt;
        if serde_json::to_vec(result)
            .map_err(ApiError::bad_request)?
            .len()
            > self.result_budget
        {
            return Err(scope_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "task_job_result_too_large",
                "deferred result exceeds retention capacity; response was not appended",
            ));
        }
        if deadline.is_some_and(|limit| tokio::time::Instant::now() >= limit) {
            return Err(super::execution::scope_commit_deadline());
        }
        entries.insert(self.id.clone(), candidate);
        Ok(())
    }
}
impl Drop for ScopeLease {
    fn drop(&mut self) {
        if let Some(history) = self.store.lock().get_mut(&self.id)
            && history.lease.as_deref() == Some(&self.lease_id)
        {
            history.lease = None;
        }
    }
}
fn history_size(messages: &[Value]) -> (usize, u64) {
    let payload = serde_json::to_string(messages).expect("messages serialize");
    (payload.len(), context::estimated_text_tokens(&payload))
}
fn total_bytes(entries: &BTreeMap<String, History>) -> usize {
    entries
        .values()
        .map(|entry| history_size(&entry.messages).0)
        .sum()
}
fn validate_size(messages: &[Value], limits: &ScopeLimits) -> Result<(), ApiError> {
    let (bytes, tokens) = history_size(messages);
    if messages.len() > limits.max_messages
        || bytes > limits.max_bytes
        || tokens > limits.max_estimated_tokens
    {
        return Err(scope_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "scope_history_limit_exceeded",
            "complete history exceeds message, byte, or estimated-token limits; compact explicitly or fork a smaller scope",
        ));
    }
    Ok(())
}
fn validate_messages(messages: &[Value]) -> Result<(), ApiError> {
    for message in messages {
        if !matches!(
            message["role"].as_str(),
            Some("system" | "user" | "assistant" | "tool")
        ) || !message["content"].is_string()
        {
            return Err(ApiError::bad_request(
                "scope messages require a supported role and string content",
            ));
        }
        if message
            .get("tool_calls")
            .is_some_and(|calls| !calls.is_array())
        {
            return Err(ApiError::bad_request("message.tool_calls must be an array"));
        }
    }
    Ok(())
}
fn scope_error(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::new(status, message).with_code(code)
}
fn not_found() -> ApiError {
    scope_error(
        StatusCode::NOT_FOUND,
        "scope_not_found",
        "scope does not exist or has expired",
    )
}
fn revision_conflict() -> ApiError {
    scope_error(
        StatusCode::CONFLICT,
        "scope_revision_conflict",
        "scope revision changed; retrieve or fork the current revision",
    )
}

pub(super) async fn create(
    State(state): State<PlatformState>,
    input: Result<Json<CreateInput>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    Ok((
        StatusCode::CREATED,
        Json(state.scopes.create(input, &state.tunables().scopes)?),
    ))
}
pub(super) async fn get(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    Query(input): Query<GetInput>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.scopes.get(
        &id,
        input.include_messages,
        &state.tunables().scopes,
    )?))
}
pub(super) async fn delete(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut entries = state.scopes.lock();
    ScopeStore::prune(&mut entries, &state.tunables().scopes);
    entries
        .remove(&id)
        .map(|_| StatusCode::NO_CONTENT)
        .ok_or_else(not_found)
}
pub(super) async fn fork(
    State(state): State<PlatformState>,
    Path(id): Path<String>,
    input: Result<Json<ForkInput>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(input) = input.map_err(|error| ApiError::invalid_task_request(&error))?;
    Ok((
        StatusCode::CREATED,
        Json(state.scopes.fork(&id, input, &state.tunables().scopes)?),
    ))
}
