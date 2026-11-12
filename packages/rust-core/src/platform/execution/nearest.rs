//! `nearest` on an embedding task: the core embeds the items (the query first, when present),
//! ranks them by cosine similarity and returns labelled matches instead of vectors, so the
//! ranking is part of the task result of a synchronous call and of a deferred job alike.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::error::ApiError;
use super::super::{PlatformState, TaskInput, TaskKind};
use super::run_session_task;

/// Most items one embedding request carries. One request of several hundred chunks crashed
/// nomic-embed-text's runner (`/tokenize: EOF`) on Ollama 0.40.13; slices run one after another
/// on the same pinned runner and cost the same.
const ITEMS_PER_SLICE: usize = 32;

/// Most items a `nearest` without a query compares pairwise: n² dot products stay bounded.
const MAX_PAIRWISE_ITEMS: usize = 4_000;

const MAX_TOP: usize = 100;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NearestRequest {
    /// Embedded first; items are ranked by closeness to it. Without it, the most similar pairs.
    pub(crate) query: Option<String>,
    /// Matches or pairs returned, 1 to 100 (default 5).
    pub(crate) top: Option<usize>,
    /// Lowest cosine similarity kept, -1 to 1.
    pub(crate) min_score: Option<f64>,
    /// One name per input item. A `path:Lstart-Lend` label is a chunk of `path`: a file is listed
    /// once, as its best chunk, and two chunks of one file are not a duplicate pair.
    pub(crate) labels: Option<Vec<String>>,
}

fn items(input: &TaskInput) -> Vec<Value> {
    match &input.input {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(item) => vec![item.clone()],
    }
}

/// Refuse a malformed `nearest` before any work is accepted, so a deferred submission fails
/// synchronously instead of as a failed job.
pub(crate) fn validate(input: &TaskInput) -> Result<(), ApiError> {
    let Some(request) = &input.nearest else {
        if input.return_embeddings {
            return Err(ApiError::bad_request(
                "return_embeddings is valid only with nearest",
            ));
        }
        return Ok(());
    };
    if !matches!(input.route.task, TaskKind::Embedding) {
        return Err(ApiError::bad_request(
            "nearest ranks embeddings; it is valid only for task \"embedding\"",
        ));
    }
    if input.scope_id.is_some() {
        return Err(ApiError::bad_request("nearest cannot run inside a scope"));
    }
    if request.top.is_some_and(|top| top == 0 || top > MAX_TOP) {
        return Err(ApiError::bad_request(format!(
            "nearest.top must be 1 to {MAX_TOP}"
        )));
    }
    if request
        .min_score
        .is_some_and(|score| !(-1.0..=1.0).contains(&score))
    {
        return Err(ApiError::bad_request(
            "nearest.min_score must be from -1 to 1",
        ));
    }
    if request.query.as_deref().is_some_and(str::is_empty) {
        return Err(ApiError::bad_request("nearest.query must be non-empty"));
    }
    let count = items(input).len();
    if let Some(labels) = &request.labels
        && labels.len() != count
    {
        return Err(ApiError::bad_request(format!(
            "nearest.labels names {} items but input has {count}",
            labels.len()
        )));
    }
    if request.query.is_none() && count > MAX_PAIRWISE_ITEMS {
        return Err(ApiError::bad_request(format!(
            "nearest without a query compares every pair; at most {MAX_PAIRWISE_ITEMS} items"
        )));
    }
    Ok(())
}

/// Run the task; with `nearest`, embed every item (in slices pinned to one runner identity)
/// and replace the vectors by the ranking.
pub(super) async fn run(
    state: PlatformState,
    mut input: TaskInput,
) -> Result<Json<Value>, ApiError> {
    validate(&input)?;
    let Some(request) = input.nearest.take() else {
        return run_session_task(State(state), Json(input)).await;
    };
    let keep_vectors = input.return_embeddings;
    let labels = request.labels.clone().unwrap_or_default();
    let mut all_items = items(&input);
    let item_count = all_items.len();
    if let Some(query) = &request.query {
        all_items.insert(0, Value::String(query.clone()));
    }
    let mut receipts: Vec<Value> = Vec::new();
    let mut vectors: Vec<Vec<f64>> = Vec::new();
    let mut identity: Option<Value> = None;
    let sliced = all_items.len() > ITEMS_PER_SLICE;
    for slice in all_items.chunks(ITEMS_PER_SLICE) {
        let mut next = input.clone();
        next.input = Some(Value::Array(slice.to_vec()));
        if let Some(pinned) = &identity {
            next.route.model = pinned["selected_model"].as_str().map(str::to_owned);
            next.expected_model_digest = pinned["model_digest"].as_str().map(str::to_owned);
            next.expected_execution_backend = pinned["backend"].as_str().map(str::to_owned);
            next.expected_upstream = pinned["upstream"].as_str().map(str::to_owned);
        }
        let first = receipts.is_empty();
        // After the first slice ran, a refusal no longer means nothing executed.
        let Json(mut receipt) = run_session_task(State(state.clone()), Json(next))
            .await
            .map_err(|error| {
                if first {
                    error
                } else {
                    error.after_partial_execution()
                }
            })?;
        let embeddings = take_vectors(&mut receipt, slice.len(), vectors.first().map(Vec::len))?;
        if sliced {
            let actual = slice_identity(&receipt, embeddings[0].len())?;
            match &identity {
                Some(pinned) if pinned != &actual => {
                    return Err(ApiError::bad_request(
                        "embedding identity mismatch between slices (model, digest, backend, endpoint, dimensions or effective options changed)",
                    ));
                }
                Some(_) => {}
                None => identity = Some(actual),
            }
        }
        vectors.extend(embeddings);
        receipts.push(receipt);
    }
    let mut result = merge(&receipts);
    let ranked = rank_blocking(vectors.clone(), labels, request, item_count).await?;
    result["nearest"] = ranked;
    let dimensions = vectors.first().map_or(0, Vec::len);
    if keep_vectors {
        result["response"]["embeddings"] = json!(vectors);
    } else {
        result["response"]["embeddings_omitted"] =
            json!({"count": vectors.len(), "dimensions": dimensions});
    }
    Ok(Json(result))
}

/// Remove and validate one request's vectors: the expected count, one dimension, finite numbers.
fn take_vectors(
    receipt: &mut Value,
    expected: usize,
    dimensions: Option<usize>,
) -> Result<Vec<Vec<f64>>, ApiError> {
    let value = receipt["response"]
        .as_object_mut()
        .and_then(|response| response.remove("embeddings"));
    validate_vectors(value.as_ref(), expected, dimensions)
}

pub(crate) fn validate_vectors(
    value: Option<&Value>,
    expected: usize,
    dimensions: Option<usize>,
) -> Result<Vec<Vec<f64>>, ApiError> {
    let Some(Value::Array(vectors)) = value else {
        return Err(ApiError::bad_request(format!(
            "embedding cardinality mismatch: expected {expected} vectors, received no vector array"
        )));
    };
    if vectors.len() != expected {
        return Err(ApiError::bad_request(format!(
            "embedding cardinality mismatch: expected {expected} vectors, received {}",
            vectors.len()
        )));
    }
    let mut dimensions = dimensions;
    let mut parsed = Vec::with_capacity(vectors.len());
    for (index, vector) in vectors.iter().enumerate() {
        let values = vector.as_array().filter(|values| !values.is_empty());
        let Some(values) = values else {
            return Err(ApiError::bad_request(format!(
                "embedding vector {index} must have a nonzero dimension"
            )));
        };
        let expected_dimensions = *dimensions.get_or_insert(values.len());
        if values.len() != expected_dimensions {
            return Err(ApiError::bad_request(format!(
                "embedding dimension mismatch at vector {index}: expected {expected_dimensions}, received {}",
                values.len()
            )));
        }
        let mut row = Vec::with_capacity(values.len());
        for value in values {
            match value.as_f64().filter(|number| number.is_finite()) {
                Some(number) => row.push(number),
                None => {
                    return Err(ApiError::bad_request(format!(
                        "embedding vector {index} contains a nonfinite or nonnumeric value"
                    )));
                }
            }
        }
        parsed.push(row);
    }
    Ok(parsed)
}

fn slice_identity(receipt: &Value, dimensions: usize) -> Result<Value, ApiError> {
    let identity = &receipt["embedding_identity"];
    let selected = receipt["route"]["selected_model"].as_str();
    let effective = &identity["effective_options"];
    let verified = identity["status"] == "verified"
        && selected.is_some_and(|model| !model.is_empty())
        && identity["model_digest"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
        && matches!(identity["backend"].as_str(), Some("cpu" | "primary"))
        && identity["upstream"].as_str().is_some_and(|u| !u.is_empty())
        && identity["dimensions"].as_u64() == u64::try_from(dimensions).ok()
        && effective["options"].is_object();
    if !verified {
        return Err(ApiError::bad_request(
            "embedding identity is uncertain: every slice requires a verified model digest, backend, upstream, dimensions and effective options receipt",
        ));
    }
    let context_known = effective["observed_context_tokens"]
        .as_u64()
        .is_some_and(|tokens| tokens > 0)
        || effective["options"]["num_ctx"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0);
    if !context_known {
        return Err(ApiError::bad_request(
            "embedding identity is uncertain: effective context was not observed or explicitly fixed",
        ));
    }
    let mut pinned = identity.clone();
    pinned["selected_model"] = json!(selected);
    Ok(pinned)
}

/// Sum a counter over every slice; one missing measurement leaves the total unknown (null).
fn sum(receipts: &[Value], field: &str, key: &str) -> Value {
    let mut total = 0_u64;
    for receipt in receipts {
        match receipt[field][key].as_u64() {
            Some(value) => total = total.saturating_add(value),
            None => return Value::Null,
        }
    }
    json!(total)
}

/// One receipt for all slices: the first receipt with the counters summed.
fn merge(receipts: &[Value]) -> Value {
    let mut merged = receipts[0].clone();
    if receipts.len() > 1 {
        for key in [
            "total_duration",
            "load_duration",
            "prompt_eval_count",
            "prompt_eval_duration",
        ] {
            if merged["response"].get(key).is_some() {
                merged["response"][key] = sum(receipts, "response", key);
            }
        }
        for key in [
            "total_duration_ns",
            "load_duration_ns",
            "prompt_tokens",
            "cached_prompt_tokens",
            "uncached_prompt_tokens",
            "prompt_duration_ns",
        ] {
            if merged["metrics"].get(key).is_some() {
                merged["metrics"][key] = sum(receipts, "metrics", key);
            }
        }
        for key in [
            "queue_wait_ms",
            "resource_wait_ms",
            "transition_wait_ms",
            "total_wait_ms",
        ] {
            if merged["admission"].get(key).is_some() {
                merged["admission"][key] = sum(receipts, "admission", key);
            }
        }
    }
    merged["embedding_requests"] = json!(receipts.len());
    merged
}

async fn rank_blocking(
    vectors: Vec<Vec<f64>>,
    labels: Vec<String>,
    request: NearestRequest,
    item_count: usize,
) -> Result<Value, ApiError> {
    debug_assert_eq!(
        vectors.len(),
        item_count + usize::from(request.query.is_some())
    );
    tokio::task::spawn_blocking(move || nearest(&vectors, &labels, &request))
        .await
        .map_err(|error| ApiError::bad_request(format!("ranking failed: {error}")))
}

// Scaling before squaring avoids overflow/underflow for finite provider values.
fn norm(vector: &[f64]) -> (f64, f64) {
    let scale = vector
        .iter()
        .fold(0.0_f64, |scale, value| scale.max(value.abs()));
    if scale == 0.0 {
        return (0.0, 0.0);
    }
    let sum: f64 = vector.iter().map(|value| (value / scale).powi(2)).sum();
    (scale, sum.sqrt())
}

fn cosine(a: &[f64], b: &[f64], norm_a: (f64, f64), norm_b: (f64, f64)) -> f64 {
    if norm_a.0 == 0.0 || norm_b.0 == 0.0 {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x / norm_a.0 / norm_a.1) * (y / norm_b.0 / norm_b.1))
        .sum();
    dot.clamp(-1.0, 1.0)
}

/// Round half up to 3 places, as the original JavaScript ranker did.
fn round(score: f64) -> f64 {
    (score * 1000.0 + 0.5).floor() / 1000.0
}

/// The file of a chunk label `path:Lstart-Lend`, or none for a plain item.
fn file_of(label: &str) -> Option<&str> {
    let (file, range) = label.rsplit_once(":L")?;
    let (start, end) = range.split_once('-')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    (digits(start) && digits(end)).then_some(file)
}

fn label_of(labels: &[String], index: usize) -> String {
    labels
        .get(index)
        .cloned()
        .unwrap_or_else(|| format!("input[{index}]"))
}

/// With a query (vector 0), the `top` items closest to it, a file once as its best chunk;
/// without one, the `top` most similar pairs, for duplicates. `labels[i]` names item i (after
/// the query). Scores are cosine similarity, rounded to 3 places.
pub(crate) fn nearest(vectors: &[Vec<f64>], labels: &[String], request: &NearestRequest) -> Value {
    let top = request.top.unwrap_or(5).clamp(1, MAX_TOP);
    let floor = request.min_score.unwrap_or(-1.0);
    let norms: Vec<_> = vectors.iter().map(|vector| norm(vector)).collect();
    if request.query.is_some() {
        let Some((query, items)) = vectors.split_first() else {
            return json!({"mode":"query","matches":[]});
        };
        let mut scored: Vec<(usize, String, f64)> = items
            .iter()
            .enumerate()
            .map(|(index, vector)| {
                (
                    index,
                    label_of(labels, index),
                    cosine(query, vector, norms[0], norms[index + 1]),
                )
            })
            .filter(|(_, _, score)| *score >= floor)
            .collect();
        // A stable sort keeps input order among equal scores.
        scored.sort_by(|left, right| right.2.total_cmp(&left.2));
        let mut seen = std::collections::BTreeSet::new();
        let matches: Vec<Value> = scored
            .into_iter()
            .filter(|(_, label, _)| {
                let file = file_of(label).unwrap_or(label).to_owned();
                seen.insert(file)
            })
            .take(top)
            .map(|(index, label, score)| json!({"index":index,"label":label,"score":round(score)}))
            .collect();
        return json!({"mode":"query","matches":matches});
    }
    let mut best: Vec<(usize, usize, f64)> = Vec::new();
    for i in 0..vectors.len() {
        for j in (i + 1)..vectors.len() {
            // Two chunks of one file are not a duplicate pair.
            let (left, right) = (label_of(labels, i), label_of(labels, j));
            if file_of(&left).is_some() && file_of(&left) == file_of(&right) {
                continue;
            }
            let score = cosine(&vectors[i], &vectors[j], norms[i], norms[j]);
            if score < floor {
                continue;
            }
            if best.len() == top && best.last().is_some_and(|last| score <= last.2) {
                continue;
            }
            best.push((i, j, score));
            best.sort_by(|a, b| b.2.total_cmp(&a.2));
            best.truncate(top);
        }
    }
    let pairs: Vec<Value> = best
        .into_iter()
        .map(|(i, j, score)| {
            json!({
                "a":{"index":i,"label":label_of(labels, i)},
                "b":{"index":j,"label":label_of(labels, j)},
                "score":round(score),
            })
        })
        .collect();
    json!({"mode":"pairs","pairs":pairs})
}

#[cfg(test)]
#[path = "nearest_tests.rs"]
mod tests;
