//! `stream:true` on `POST /_freellama/v1/tasks`: one managed task as server-sent events.
//!
//! The task runs exactly as a non-streaming one (route, fallback, admission, memory gate,
//! receipt); the stream adds what the caller would otherwise wait for in silence:
//!
//! | Event | When | Data |
//! |---|---|---|
//! | `admission` | queued, waiting for memory, loading, running | `{status, reason, detail?}` |
//! | `route` | the model is chosen, before dispatch (once per fallback attempt) | `{selected_model, backend}` |
//! | `delta` | each generated piece | `{content}` or `{thinking}` |
//! | `done` | finished | the receipt the non-streaming call returns |
//! | `error` | refused or failed | the error body, `status` and `retry_safe` |
//!
//! Fallback models advance only before generation starts, as without streaming. A client that
//! disconnects aborts the task and drops the upstream request to request cancellation; the
//! work is not replayed.
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use http_body::Frame;
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::AbortHandle};

use super::error::ApiError;
use super::jobs::{JobProgress, JobStatus};
use super::{PlatformState, RouteInput, TaskInput, TaskKind};

/// What a running streamed task reports before its receipt.
#[derive(Debug)]
pub(crate) enum TaskEvent {
    Phase {
        status: JobStatus,
        reason: &'static str,
        detail: Option<Value>,
    },
    Route {
        model: String,
        backend: String,
    },
    Content(String),
    Thinking(String),
}

pub(crate) type EventSink = mpsc::Sender<TaskEvent>;

const EVENT_CAPACITY: usize = 32;
const FRAME_CAPACITY: usize = 16;

/// The pipeline's progress reporter: a deferred job's status, a stream's events, or both.
#[derive(Debug, Clone)]
pub(crate) struct Progress {
    job: Option<JobProgress>,
    events: Option<EventSink>,
}

impl Progress {
    pub(crate) fn new(job: Option<JobProgress>, events: Option<EventSink>) -> Option<Self> {
        (job.is_some() || events.is_some()).then_some(Self { job, events })
    }

    pub(crate) fn update(&self, status: JobStatus, reason: &'static str, detail: Option<Value>) {
        if let Some(events) = &self.events {
            // Status is best effort under backpressure; generated text uses awaited sends.
            let _ = events.try_send(TaskEvent::Phase {
                status,
                reason,
                detail: detail.clone(),
            });
        }
        if let Some(job) = &self.job {
            job.update(status, reason, detail);
        }
    }

    pub(crate) async fn route(&self, model: &str, backend: &str) {
        if let Some(events) = &self.events {
            let _ = events
                .send(TaskEvent::Route {
                    model: model.to_owned(),
                    backend: backend.to_owned(),
                })
                .await;
        }
        if let Some(job) = &self.job {
            job.route(model, backend);
        }
    }
}

/// A deferred chat job's token counter: generated pieces arrive here and the job's status shows
/// `{generated_tokens, tokens_per_second}` at most about once a second, so a caller polling the
/// job (MCP `run_task` progress) sees liveness without receiving the text.
pub(crate) fn job_token_counter(job: JobProgress) -> EventSink {
    let (events, mut received) = mpsc::channel::<TaskEvent>(EVENT_CAPACITY);
    tokio::spawn(async move {
        let mut tokens = 0_u64;
        let mut started = None;
        let mut reported = None::<std::time::Instant>;
        while let Some(event) = received.recv().await {
            if !matches!(event, TaskEvent::Content(_) | TaskEvent::Thinking(_)) {
                continue;
            }
            tokens += 1;
            let now = std::time::Instant::now();
            let first = *started.get_or_insert(now);
            if reported.is_none_or(|at| now - at >= std::time::Duration::from_secs(1)) {
                reported = Some(now);
                let elapsed = (now - first).as_secs_f64();
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a progress rate, not an exact count"
                )]
                let rate = (elapsed > 0.0).then(|| (tokens as f64 / elapsed * 10.0).round() / 10.0);
                job.update(
                    JobStatus::Running,
                    "generating",
                    Some(json!({"generated_tokens": tokens, "tokens_per_second": rate})),
                );
            }
        }
    });
    events
}

/// Tasks whose answer is generated text, so there is something to stream.
fn streamable(task: TaskKind) -> bool {
    matches!(
        task,
        TaskKind::Completion
            | TaskKind::Coding
            | TaskKind::CodeRepair
            | TaskKind::Vision
            | TaskKind::LongContext
    )
}

/// Refuse a stream that could not carry tokens, before anything is admitted.
pub(crate) fn validate(input: &TaskInput) -> Result<(), ApiError> {
    let refuse = |why: &str| {
        Err(ApiError::bad_request(format!("stream: {why}")).with_code("stream_unsupported"))
    };
    let RouteInput { task, .. } = &input.route;
    if !streamable(*task) {
        return refuse(
            "only generating tasks stream (completion, coding, code_repair, vision, long_context)",
        );
    }
    if input.defer {
        return refuse("a deferred task reports progress through task jobs instead");
    }
    if input.tools.is_some() {
        return refuse("tool calls arrive whole; send the task without stream");
    }
    if input.questions.is_some() || input.input.is_some() || input.nearest.is_some() {
        return refuse("decision and embedding tasks return no generated text");
    }
    if !matches!(input.operation, super::ManagedOperation::Generate) {
        return refuse("a warm generates nothing");
    }
    Ok(())
}

/// One server-sent event.
fn frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn event_frame(event: TaskEvent) -> (Bytes, bool) {
    match event {
        TaskEvent::Phase {
            status,
            reason,
            detail,
        } => {
            let mut data = json!({"status": status, "reason": reason});
            if let Some(detail) = detail {
                data["detail"] = detail;
            }
            (frame("admission", &data), false)
        }
        TaskEvent::Route { model, backend } => (
            frame(
                "route",
                &json!({"selected_model": model, "backend": backend}),
            ),
            false,
        ),
        TaskEvent::Content(text) => (frame("delta", &json!({"content": text})), true),
        TaskEvent::Thinking(text) => (frame("delta", &json!({"thinking": text})), true),
    }
}

fn error_frame(error: ApiError, generated: bool) -> Bytes {
    let mut data = error.into_batch_result(String::new());
    if let Some(object) = data.as_object_mut() {
        object.remove("id");
        object.remove("ok");
    }
    // Once tokens went out, generation started: a retry could repeat work the caller saw.
    let status = data["status"].as_u64().unwrap_or(500);
    let retry_safe = !generated
        && data["retry_safe"]
            .as_bool()
            .unwrap_or(matches!(status, 429 | 503));
    data["retry_safe"] = json!(retry_safe);
    frame("error", &data)
}

/// Start the task and answer with its event stream.
pub(crate) fn respond(state: PlatformState, mut input: TaskInput) -> Response {
    let (frames, receiver) = mpsc::channel::<Bytes>(FRAME_CAPACITY);
    let (events, mut task_events) = mpsc::channel::<TaskEvent>(EVENT_CAPACITY);
    input.stream_events = Some(events);
    // The execution task must keep polling its deadline while a connected reader stops consuming
    // frames. A separate bounded forwarder provides backpressure without suspending that timer.
    input
        .timeout_seconds
        .get_or_insert_with(|| super::platform_task_timeout().as_secs());
    let mut work = tokio::spawn(super::execution::execute_task(State(state), Json(input)));
    let cancel_work = AbortTaskOnDrop(work.abort_handle());
    let task = tokio::spawn(async move {
        let _cancel_work = cancel_work;
        let mut generated = false;
        let result = loop {
            tokio::select! {
                biased;
                Some(event) = task_events.recv() => {
                    let (bytes, delta) = event_frame(event);
                    generated |= delta;
                    if frames.send(bytes).await.is_err() {
                        return;
                    }
                }
                result = &mut work => break result.unwrap_or_else(|error| {
                    Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error)
                        .with_upstream_response("task_worker_failed", json!({"done":false}), None))
                }),
            }
        };
        while let Ok(event) = task_events.try_recv() {
            let (bytes, delta) = event_frame(event);
            generated |= delta;
            if frames.send(bytes).await.is_err() {
                return;
            }
        }
        let last = match result {
            Ok(Json(receipt)) => frame("done", &receipt),
            Err(error) => error_frame(error, generated),
        };
        let _ = frames.send(last).await;
    });
    let mut response = Response::new(Body::new(EventBody {
        frames: receiver,
        task: task.abort_handle(),
    }));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Reverse proxies otherwise buffer the stream until it ends.
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// The response body: frames as the task produces them. Dropping it (the client went away)
/// aborts the task, dropping its upstream request and releasing local execution capacity.
struct EventBody {
    frames: mpsc::Receiver<Bytes>,
    task: AbortHandle,
}

/// Aborting a forwarder must not detach the task that owns admission and resource reservations.
struct AbortTaskOnDrop(AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for EventBody {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl http_body::Body for EventBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.frames
            .poll_recv(context)
            .map(|frame| frame.map(|bytes| Ok(Frame::data(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_server_sent_events() {
        let (bytes, delta) = event_frame(TaskEvent::Content("Hel\nlo".into()));
        assert!(delta);
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "event: delta\ndata: {\"content\":\"Hel\\nlo\"}\n\n"
        );
        let (bytes, delta) = event_frame(TaskEvent::Phase {
            status: JobStatus::WaitingForResources,
            reason: "host_pressure",
            detail: None,
        });
        assert!(!delta);
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .starts_with("event: admission\ndata: {\"reason\":\"host_pressure\",\"status\":\"waiting_for_resources\"}")
        );
    }

    #[test]
    fn an_error_after_generation_started_is_not_retry_safe() {
        let refused = || {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "no room").with_code("admission_timeout")
        };
        let before = error_frame(refused(), false);
        assert!(
            std::str::from_utf8(&before)
                .unwrap()
                .contains("\"retry_safe\":true")
        );
        let after = error_frame(refused(), true);
        let text = std::str::from_utf8(&after).unwrap();
        assert!(text.starts_with("event: error\n"));
        assert!(text.contains("\"retry_safe\":false"));
        assert!(text.contains("\"status\":503"));
    }
}
