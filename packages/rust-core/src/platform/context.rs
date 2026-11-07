//! Prompt-aware sizing for managed text requests. Estimates are never tokenizer proof.

use serde_json::{Value, json};

use super::routing::VisionTokenBounds;
use super::{ApiError, CatalogModel, RouteDecision, TaskInput, TaskKind};

const AUTO_CONTEXT_LIMIT: u64 = 32_768;
pub(super) const TEMPLATE_MARGIN: u64 = 512;

/// Use UTF-8 bytes so non-ASCII inputs cannot receive the same estimate as equal-length ASCII.
/// Three bytes/token is a conservative heuristic for ordinary text/code, not an exact bound.
pub(super) fn estimated_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

/// The most bytes one token is assumed to cover; a learned ratio is clamped to it.
const MAX_BYTES_PER_TOKEN_MILLI: u64 = 8000;

/// Machine-readable code for a prompt that cannot fit the context window it would run in.
const CONTEXT_WINDOW_EXCEEDED: &str = "context_window_exceeded";

/// Bytes per token (times 1000) used for a model: its learned ratio less a 10% margin, so an
/// estimate errs toward more tokens, or the fixed 3 until it has been measured.
fn bytes_per_token_milli(model: &CatalogModel) -> u64 {
    model.bytes_per_token_milli.map_or(3000, |milli| {
        (milli / 10 * 9).clamp(1000, MAX_BYTES_PER_TOKEN_MILLI)
    })
}

/// Tokens in `text` for `model`, from its learned ratio when known; rounded up.
pub(super) fn estimated_model_tokens(text: &str, model: &CatalogModel) -> u64 {
    (text.len() as u64)
        .saturating_mul(1000)
        .div_ceil(bytes_per_token_milli(model))
}

/// Image token bounds a model's own `/api/show` metadata declares, or `None` when it declares
/// none. Read only from `clip.vision.*` and `<architecture>.vision.*`; nothing is inferred from a
/// model's name or from `image_size` alone, which dynamic-resolution encoders do not honour.
///
/// - `<architecture>.mm.tokens_per_image`: an exact figure, both bounds.
/// - Otherwise tokens per image are pixels / (`patch_size` * `spatial_merge_size`)^2. The lower
///   bound needs the declared minimum pixels (rounded down), the upper bound the declared
///   maximum pixels (rounded up). A missing patch size, merge size or pixel limit leaves that
///   bound unknown; slice-based encoders that declare no pixel limits get no bounds.
pub(super) fn vision_token_bounds(show: &Value) -> Option<VisionTokenBounds> {
    let info = show.get("model_info")?.as_object()?;
    let architecture = info.get("general.architecture").and_then(Value::as_str);
    // A figure two metadata families declare differently is ambiguous, so it is unknown.
    let field = |suffixes: &[&str]| -> Option<u64> {
        let mut found = None;
        for prefix in [Some("clip"), architecture].into_iter().flatten() {
            for suffix in suffixes {
                let Some(value) = info.get(&format!("{prefix}.{suffix}")) else {
                    continue;
                };
                let value = value.as_u64().filter(|value| *value > 0)?;
                if found.is_some_and(|known| known != value) {
                    return None;
                }
                found = Some(value);
            }
        }
        found
    };
    if let Some(exact) = architecture
        .and_then(|architecture| info.get(&format!("{architecture}.mm.tokens_per_image")))
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
    {
        return Some(VisionTokenBounds {
            min_per_image: Some(exact),
            max_per_image: Some(exact),
        });
    }
    let side =
        field(&["vision.patch_size"])?.checked_mul(field(&["vision.spatial_merge_size"])?)?;
    let unit = side.checked_mul(side)?;
    let bounds = VisionTokenBounds {
        min_per_image: field(&["vision.image_min_pixels", "vision.min_pixels"])
            .map(|pixels| pixels / unit)
            .filter(|tokens| *tokens > 0),
        max_per_image: field(&["vision.image_max_pixels", "vision.max_pixels"])
            .map(|pixels| pixels.div_ceil(unit)),
    };
    (bounds.min_per_image.is_some() || bounds.max_per_image.is_some()).then_some(bounds)
}

/// Whether routing's cached `/api/show` projection must keep this `model_info` key for
/// [`vision_token_bounds`].
pub(super) fn is_vision_bound_key(key: &str, architecture: Option<&str>) -> bool {
    key.starts_with("clip.vision.")
        || architecture
            .and_then(|architecture| key.strip_prefix(architecture))
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|suffix| suffix.starts_with("vision.") || suffix == "mm.tokens_per_image")
}

/// Media in a request, as far as it can be counted.
struct Media {
    images: u64,
    /// Audio or content of a kind that cannot be counted: no total can be estimated.
    opaque: bool,
}

fn media_inventory(input: &TaskInput) -> Media {
    let mut media = Media {
        images: input
            .images
            .as_ref()
            .map_or(0, |images| images.len() as u64),
        opaque: false,
    };
    for message in &input.messages {
        match message.get("images") {
            None => {}
            Some(Value::Array(images)) => media.images += images.len() as u64,
            Some(_) => media.opaque = true,
        }
        media.opaque |= message.get("audio").is_some();
        match message.get("content") {
            None | Some(Value::String(_)) => {}
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text" | "input_text") => {}
                        Some("image" | "image_url" | "input_image") => media.images += 1,
                        _ => media.opaque = true,
                    }
                }
            }
            Some(_) => media.opaque = true,
        }
    }
    media
}

fn has_unestimated_modality(input: &TaskInput) -> bool {
    matches!(input.route.task, TaskKind::Embedding | TaskKind::Vision)
        || input
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
        || input.messages.iter().any(|message| {
            message.get("images").is_some()
                || message.get("audio").is_some()
                || message
                    .get("content")
                    .is_some_and(|content| !content.is_string())
        })
}

/// Prompt-free work estimate for completion planning. It never treats encoded media as text.
/// Embedding input is text-only; generation includes schemas and a conservative template margin.
pub(super) fn estimated_work_tokens(input: &TaskInput, model: &CatalogModel) -> Option<u64> {
    if matches!(input.route.task, TaskKind::Embedding) {
        return match input.input.as_ref()? {
            Value::String(text) => Some(estimated_model_tokens(text, model)),
            Value::Array(items) => items.iter().try_fold(0_u64, |total, item| {
                total.checked_add(estimated_model_tokens(item.as_str()?, model))
            }),
            _ => None,
        };
    }
    if has_unestimated_modality(input) || matches!(input.route.task, TaskKind::Decision) {
        return None;
    }
    let payload = if input.messages.is_empty() {
        input.prompt.clone().unwrap_or_default()
    } else {
        serde_json::to_string(&input.messages).ok()?
    };
    let mut tokens = estimated_model_tokens(&payload, model).saturating_add(TEMPLATE_MARGIN);
    for schema in [input.tools.as_ref(), input.request_options.format.as_ref()]
        .into_iter()
        .flatten()
    {
        tokens = tokens.saturating_add(estimated_model_tokens(&schema.to_string(), model));
    }
    Some(tokens)
}

/// The text a message contributes: its string content or the `text` parts of structured content.
/// Encoded media is dropped; it needs a model-specific estimate, not byte counting.
fn text_projection(message: &Value) -> Value {
    let Some(fields) = message.as_object() else {
        return message.clone();
    };
    Value::Object(
        fields
            .iter()
            .filter_map(|(key, value)| {
                let value = match (key.as_str(), value) {
                    ("content", Value::Array(parts)) => Value::Array(
                        parts
                            .iter()
                            .filter(|part| {
                                matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("text" | "input_text")
                                )
                            })
                            .cloned()
                            .collect(),
                    ),
                    ("content", Value::String(_)) => value.clone(),
                    ("images" | "audio" | "content", _) => return None,
                    _ => value.clone(),
                };
                Some((key.clone(), value))
            })
            .collect(),
    )
}

/// Project only the execution estimate; the caller's payload and stored history stay intact.
fn text_only_payload(input: &TaskInput) -> String {
    if input.messages.is_empty() {
        return input.prompt.clone().unwrap_or_default();
    }
    let messages: Vec<Value> = input.messages.iter().map(text_projection).collect();
    serde_json::to_string(&messages).expect("messages serialize")
}

/// Lower bound refusal and receipt for an unscoped request carrying images, when the model
/// declares image token bounds. Context sizing is left to the profile: the declared maximum is a
/// limit of the encoder, not what a typical image costs. Audio, uncountable content and models
/// without declared bounds stay unknown.
fn size_media_profile(
    input: &TaskInput,
    decision: &RouteDecision,
    model: &CatalogModel,
) -> Result<Value, ApiError> {
    let media = media_inventory(input);
    let Some(bounds) = model.vision_tokens.filter(|_| {
        media.images > 0 && !media.opaque && !matches!(input.route.task, TaskKind::Embedding)
    }) else {
        return Ok(
            json!({"mode": "profile", "reason": "modality_requires_model_specific_estimator"}),
        );
    };
    let mut text = estimated_model_tokens(&text_only_payload(input), model);
    for schema in [input.tools.as_ref(), input.request_options.format.as_ref()]
        .into_iter()
        .flatten()
    {
        text = text.saturating_add(estimated_model_tokens(&schema.to_string(), model));
    }
    let image_min = media
        .images
        .saturating_mul(bounds.min_per_image.unwrap_or(0));
    let image_max = bounds
        .max_per_image
        .map(|tokens| media.images.saturating_mul(tokens));
    let minimum = text
        .saturating_add(image_min)
        .saturating_add(TEMPLATE_MARGIN);
    // A caller's cap is a promise that must fit; the model's own window is a hard limit.
    let requested_output = decision.options["num_predict"].as_u64().unwrap_or(0);
    if let Some(window) = model.advertised_context
        && minimum.saturating_add(requested_output) > window
    {
        return Err(ApiError::bad_request(format!(
            "text plus the declared minimum for {} image(s) requires at least {} context tokens, above the model's context {window}; reduce the images or text",
            media.images,
            minimum.saturating_add(requested_output)
        )));
    }
    Ok(json!({
        "mode": "profile", "reason": "vision_token_bounds_declared_by_model",
        "estimator": "utf8_bytes_div_3_plus_declared_vision_bounds",
        "estimate_scope": "text_plus_declared_image_bounds", "images": media.images,
        "estimated_text_input_tokens": text, "estimated_input_tokens": null,
        "estimated_input_tokens_min": minimum.saturating_sub(TEMPLATE_MARGIN),
        "estimated_input_tokens_max": image_max.map(|images| text.saturating_add(images)),
        "template_margin_tokens": TEMPLATE_MARGIN,
        "total_fit_verified": false, "exact_token_count": false,
    }))
}

fn size_scoped_explicit_context(
    input: &TaskInput,
    decision: &RouteDecision,
    unestimated_modality: bool,
) -> Result<Value, ApiError> {
    let payload = if unestimated_modality {
        text_only_payload(input)
    } else {
        serde_json::to_string(&input.messages).expect("messages serialize")
    };
    let mut estimated = estimated_text_tokens(&payload);
    for schema in [input.tools.as_ref(), input.request_options.format.as_ref()]
        .into_iter()
        .flatten()
    {
        estimated = estimated.saturating_add(estimated_text_tokens(&schema.to_string()));
    }
    let available = decision.options["num_ctx"].as_u64().unwrap_or_default();
    // As for unscoped work: a caller's cap must fit, the expected output gives way to the history.
    let room = available.saturating_sub(estimated.saturating_add(TEMPLATE_MARGIN));
    let output = decision.options["num_predict"]
        .as_u64()
        .unwrap_or_else(|| decision.output_reserve_tokens.min(room));
    let required = estimated
        .saturating_add(output)
        .saturating_add(TEMPLATE_MARGIN);
    if required > available {
        let estimate_scope = if unestimated_modality {
            "scoped text and metadata"
        } else {
            "complete scoped history"
        };
        return Err(ApiError::bad_request(format!(
            "{estimate_scope} plus output reserve requires approximately {required} tokens, above configured context {available}; compact explicitly or increase context_tokens"
        )));
    }
    if unestimated_modality {
        return Ok(json!({
            "mode":"scope_explicit_multimodal", "tokens":available,
            "estimator":"utf8_bytes_div_3", "estimate_scope":"text_and_metadata_only",
            "estimated_text_input_tokens":estimated, "estimated_input_tokens":null,
            "modality_tokens":null, "reason":"modality_requires_model_specific_estimator",
            "output_reserve_tokens":output, "template_margin_tokens":TEMPLATE_MARGIN,
            "total_fit_verified":false, "exact_token_count":false,
        }));
    }
    Ok(
        json!({"mode":"scope_explicit_estimate","tokens":available,"estimated_input_tokens":estimated,"output_reserve_tokens":output,"template_margin_tokens":TEMPLATE_MARGIN,"exact_token_count":false}),
    )
}

pub(super) fn size_context(
    input: &TaskInput,
    decision: &mut RouteDecision,
    model: &CatalogModel,
) -> Result<Value, ApiError> {
    if matches!(input.operation, super::ManagedOperation::Warm) {
        return Ok(json!({"mode":"warm_profile","tokens":decision.options["num_ctx"]}));
    }
    let unestimated_modality = has_unestimated_modality(input);
    if input.scope_id.is_some() && unestimated_modality && input.route.context_tokens.is_none() {
        return Err(ApiError::bad_request(
            "scoped multimodal history requires explicit context_tokens; total token cost requires a model-specific estimator",
        ));
    }
    if input.scope_id.is_some() && input.route.context_tokens.is_some() {
        return size_scoped_explicit_context(input, decision, unestimated_modality);
    }
    let capped = decision.options["num_predict"].as_u64();
    if input.route.context_tokens.is_some() {
        return size_explicit_context(input, decision, unestimated_modality, capped);
    }
    // Image/audio token cost is model-dependent. Base64 bytes are not text tokens.
    if input.scope_id.is_none() && unestimated_modality {
        return size_media_profile(input, decision, model);
    }
    let estimated = payload_tokens(input, |text| estimated_model_tokens(text, model));
    let cap = model
        .advertised_context
        .unwrap_or_else(|| decision.options["num_ctx"].as_u64().unwrap_or(2048))
        .min(AUTO_CONTEXT_LIMIT);
    // A caller's `num_predict` is a promise that must fit. The expected output is only an
    // estimate: it gives way to the prompt, so a long prompt still runs in the model's window.
    let room = cap.saturating_sub(estimated.saturating_add(TEMPLATE_MARGIN));
    let output = capped.unwrap_or_else(|| decision.output_reserve_tokens.min(room));
    let required = estimated
        .saturating_add(output)
        .saturating_add(TEMPLATE_MARGIN);
    if required > cap {
        return Err(ApiError::bad_request(format!(
            "estimated prompt plus requested output requires {required} context tokens, above the automatic cap {cap}; compact the supplied history or set an explicit context_tokens supported by the model"
        ))
        .with_code(CONTEXT_WINDOW_EXCEEDED));
    }
    let selected = [2048, 4096, 8192, 16_384, AUTO_CONTEXT_LIMIT]
        .into_iter()
        .find(|bucket| *bucket >= required)
        .unwrap_or(cap)
        .min(cap);
    decision.options["num_ctx"] = json!(selected);
    decision
        .reasons
        .retain(|reason| reason != "context_clamped_to_advertised");
    decision
        .reasons
        .push("context_sized_from_execution_payload".to_owned());
    Ok(json!({
        "mode": "prompt_estimate",
        "estimator": if model.bytes_per_token_milli.is_some() { "utf8_bytes_per_learned_token_ratio" } else { "utf8_bytes_div_3" },
        "bytes_per_token_milli": bytes_per_token_milli(model),
        "bytes_per_token_samples": model.bytes_per_token_samples,
        "estimated_input_tokens": estimated, "output_reserve_tokens": output,
        "output_capped": capped.is_some(),
        "template_margin_tokens": TEMPLATE_MARGIN, "tokens": selected,
        "automatic_cap_tokens": cap, "exact_token_count": false,
    }))
}

/// Text, tool and format-schema tokens of an unscoped request, counted by `tokens`.
fn payload_tokens(input: &TaskInput, tokens: impl Fn(&str) -> u64) -> u64 {
    let payload = if input.messages.is_empty() {
        input.prompt.clone().unwrap_or_default()
    } else {
        serde_json::to_string(&input.messages).expect("JSON messages serialize")
    };
    [input.tools.as_ref(), input.request_options.format.as_ref()]
        .into_iter()
        .flatten()
        .fold(tokens(&payload), |total, schema| {
            total.saturating_add(tokens(&schema.to_string()))
        })
}

/// A caller-chosen window is kept as given. It is refused only when the text alone certainly
/// overflows it: counted at the most bytes a token is assumed to cover, so an estimate never
/// refuses a prompt that would have fit, and the model is not loaded just for Ollama to reject
/// the request. Media and the expected output are not counted; a caller's output cap is.
fn size_explicit_context(
    input: &TaskInput,
    decision: &RouteDecision,
    unestimated_modality: bool,
    capped: Option<u64>,
) -> Result<Value, ApiError> {
    let window = decision.options["num_ctx"].as_u64().unwrap_or_default();
    let fewest = |text: &str| {
        (text.len() as u64)
            .saturating_mul(1000)
            .div_ceil(MAX_BYTES_PER_TOKEN_MILLI)
    };
    let minimum = if unestimated_modality {
        fewest(&text_only_payload(input))
    } else {
        payload_tokens(input, fewest)
    };
    let required = minimum.saturating_add(capped.unwrap_or(0));
    if window > 0 && required > window {
        return Err(ApiError::bad_request(format!(
            "the prompt needs at least {required} tokens (text counted at {} bytes per token{}), above context_tokens {window}; shorten the input or raise context_tokens",
            MAX_BYTES_PER_TOKEN_MILLI / 1000,
            if capped.is_some() { ", plus the output cap" } else { "" },
        ))
        .with_code(CONTEXT_WINDOW_EXCEEDED));
    }
    Ok(json!({
        "mode": "explicit", "tokens": decision.options["num_ctx"],
        "minimum_input_tokens": minimum, "total_fit_verified": false, "exact_token_count": false,
    }))
}

/// Reuse a loaded runner's context window when it already covers this request.
///
/// Ollama reloads a runner whose `num_ctx` differs from the request's, so bucketing each request
/// independently (2k/4k/8k/...) turned a warm model into a reload whenever consecutive prompts
/// landed in different buckets, and back again. Returns the reused size, or `None` when the
/// loaded runner is a different revision or smaller than the request needs.
pub(super) fn reuse_resident_context(
    decision: &mut RouteDecision,
    model: &CatalogModel,
    loaded: Option<&Value>,
) -> Option<u64> {
    let loaded = loaded?;
    let requested = decision.options.get("num_ctx").and_then(Value::as_u64)?;
    let loaded_context = loaded.get("context_length").and_then(Value::as_u64)?;
    let same_revision = match (
        model.digest.as_deref(),
        loaded.get("digest").and_then(Value::as_str),
    ) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    };
    if !same_revision || loaded_context < requested || loaded_context == requested {
        return None;
    }
    decision.options["num_ctx"] = json!(loaded_context);
    decision
        .reasons
        .push("context_reused_from_resident_runner".to_owned());
    Some(loaded_context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{SessionAffinity, select_route};

    fn request(prompt: &str) -> (TaskInput, CatalogModel, RouteDecision) {
        let input: TaskInput = serde_json::from_value(json!({
            "task": "completion", "objective": "fastest", "prompt": prompt
        }))
        .unwrap();
        let model: CatalogModel = serde_json::from_value(json!({
            "name": "test", "size": 1, "capabilities": ["completion"],
            "advertised_context": 32768, "resident": false,
            "benchmark": {}, "policy_rank": {}
        }))
        .unwrap();
        let decision = select_route(
            &input.route,
            std::slice::from_ref(&model),
            &SessionAffinity::default(),
        )
        .unwrap();
        (input, model, decision)
    }

    #[test]
    fn a_learned_token_ratio_replaces_the_fixed_one_with_a_safety_margin() {
        let (_, mut model, _) = request("x");
        let text = "a".repeat(9_000);
        assert_eq!(
            estimated_model_tokens(&text, &model),
            3_000,
            "unmeasured: 3 bytes per token"
        );
        model.bytes_per_token_milli = Some(4_000);
        // 4.0 bytes per token less 10% is 3.6: 9000 / 3.6 = 2500.
        assert_eq!(estimated_model_tokens(&text, &model), 2_500);
        model.bytes_per_token_milli = Some(100_000);
        assert_eq!(
            estimated_model_tokens(&text, &model),
            1_125,
            "capped at 8 bytes per token"
        );
    }

    #[test]
    fn a_thinking_model_gets_room_to_think_before_it_answers() {
        let input: TaskInput = serde_json::from_value(json!({
            "task": "completion", "objective": "fastest", "prompt": "hello"
        }))
        .unwrap();
        let model: CatalogModel = serde_json::from_value(json!({
            "name": "thinker", "size": 1, "capabilities": ["completion", "thinking"],
            "advertised_context": 32768, "resident": false,
            "benchmark": {}, "policy_rank": {}
        }))
        .unwrap();
        let decision = select_route(
            &input.route,
            std::slice::from_ref(&model),
            &SessionAffinity::default(),
        )
        .unwrap();
        // Room to reason is expected for context sizing; no cap is sent.
        assert_eq!(decision.output_reserve_tokens, 2048);
        assert!(decision.options.get("num_predict").is_none());
    }

    #[test]
    fn short_text_uses_smallest_bucket_and_sends_no_output_cap() {
        let (input, model, mut decision) = request("hello");
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(decision.options["num_ctx"], 2048);
        assert!(decision.options.get("num_predict").is_none());
        assert_eq!(receipt["output_reserve_tokens"], 512);
        assert_eq!(receipt["output_capped"], false);
        assert_eq!(receipt["exact_token_count"], false);
    }

    #[test]
    fn the_expected_output_gives_way_to_a_long_prompt_but_a_cap_must_fit() {
        let (mut input, mut model, mut decision) = request("hello");
        model.advertised_context = Some(4096);
        // About 3,400 prompt tokens plus the template margin leave less than the expected 512.
        input.prompt = Some("x".repeat(3 * 3_400));
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(decision.options["num_ctx"], 4096);
        assert!(receipt["output_reserve_tokens"].as_u64().unwrap() < 512);
        decision.options["num_predict"] = json!(512);
        assert!(size_context(&input, &mut decision, &model).is_err());
    }

    #[test]
    fn schema_and_output_reserve_are_counted_and_oversize_fails() {
        let (mut input, model, mut decision) = request("hello");
        input.tools = Some(json!({"description": "x".repeat(12_000)}));
        size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(decision.options["num_ctx"], 8192);
        input.prompt = Some("x".repeat(100_000));
        assert!(size_context(&input, &mut decision, &model).is_err());
    }

    #[test]
    fn an_explicit_window_is_refused_only_when_the_text_certainly_overflows_it() {
        let (mut input, model, mut decision) = request("hello");
        input.route.context_tokens = Some(8192);
        decision.options["num_ctx"] = json!(8192);
        // 40,000 bytes is at least 5,000 tokens at 8 bytes per token: it may fit, so it runs.
        input.prompt = Some("x".repeat(40_000));
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(receipt["mode"], "explicit");
        assert_eq!(receipt["minimum_input_tokens"], 5_000);
        // A caller's output cap must fit beside that minimum.
        decision.options["num_predict"] = json!(4_000);
        let capped = size_context(&input, &mut decision, &model).unwrap_err();
        assert_eq!(
            serde_json::to_value(&capped.body).unwrap()["code"],
            "context_window_exceeded"
        );
        decision
            .options
            .as_object_mut()
            .unwrap()
            .remove("num_predict");
        // 200,000 bytes is at least 25,000 tokens: certainly over 8,192, refused before loading.
        input.prompt = Some("word ".repeat(40_000));
        let error = size_context(&input, &mut decision, &model).unwrap_err();
        assert_eq!(
            serde_json::to_value(&error.body).unwrap()["code"],
            "context_window_exceeded"
        );
        // Text beside an image is still counted.
        input.prompt = None;
        input.messages =
            vec![json!({"role":"user","content":"word ".repeat(40_000),"images":["aW1hZ2U="]})];
        assert!(size_context(&input, &mut decision, &model).is_err());
    }

    #[test]
    fn explicit_context_and_multimodal_profiles_are_preserved() {
        let (mut input, model, mut decision) = request("hello");
        input.route.context_tokens = Some(16384);
        assert_eq!(
            size_context(&input, &mut decision, &model).unwrap()["mode"],
            "explicit"
        );
        assert_eq!(decision.options["num_ctx"], 16384);
        input.route.context_tokens = None;
        input.images = Some(vec!["aW1hZ2U=".into()]);
        assert_eq!(
            size_context(&input, &mut decision, &model).unwrap()["mode"],
            "profile"
        );
        assert_eq!(decision.options["num_ctx"], 16384);
    }

    fn show(info: Value) -> Value {
        let mut wrapper = json!({});
        wrapper["model_info"] = info;
        wrapper
    }

    #[test]
    fn vision_bounds_come_only_from_declared_metadata() {
        // Pixel limits divided by (patch_size * spatial_merge_size)^2: min rounds down, max up.
        let clip = show(json!({
            "general.architecture": "lfm2",
            "clip.vision.patch_size": 16, "clip.vision.spatial_merge_size": 2,
            "clip.vision.image_min_pixels": 65_536, "clip.vision.image_max_pixels": 16_777_216,
            "clip.vision.image_size": 512,
        }));
        assert_eq!(
            vision_token_bounds(&clip),
            Some(VisionTokenBounds {
                min_per_image: Some(64),
                max_per_image: Some(16_384)
            })
        );
        let architecture_scoped = show(json!({
            "general.architecture": "glmocr",
            "glmocr.vision.patch_size": 14, "glmocr.vision.spatial_merge_size": 2,
            "glmocr.vision.min_pixels": 12_544, "glmocr.vision.max_pixels": 9_633_793,
        }));
        assert_eq!(
            vision_token_bounds(&architecture_scoped),
            Some(VisionTokenBounds {
                min_per_image: Some(16),
                max_per_image: Some(12_289)
            })
        );
        let exact = show(json!({
            "general.architecture": "gemma3", "gemma3.mm.tokens_per_image": 256,
            "gemma3.vision.image_size": 896,
        }));
        assert_eq!(
            vision_token_bounds(&exact),
            Some(VisionTokenBounds {
                min_per_image: Some(256),
                max_per_image: Some(256)
            })
        );
    }

    #[test]
    fn undeclared_or_ambiguous_vision_metadata_stays_unknown() {
        // Slice-based encoder: image size and patch size alone say nothing about token count.
        let slices = show(json!({
            "general.architecture": "minicpmv",
            "clip.vision.image_size": 448, "clip.vision.patch_size": 14,
            "clip.vision.max_slice_nums": 9,
        }));
        assert_eq!(vision_token_bounds(&slices), None);
        let no_merge = show(json!({
            "general.architecture": "x", "clip.vision.patch_size": 16,
            "clip.vision.image_max_pixels": 1_048_576,
        }));
        assert_eq!(vision_token_bounds(&no_merge), None);
        let conflicting = show(json!({
            "general.architecture": "x", "clip.vision.patch_size": 16,
            "x.vision.patch_size": 14, "clip.vision.spatial_merge_size": 2,
            "clip.vision.image_max_pixels": 1_048_576,
        }));
        assert_eq!(vision_token_bounds(&conflicting), None);
        let audio_only = show(json!({
            "general.architecture": "x", "clip.audio.num_mel_bins": 128,
        }));
        assert_eq!(vision_token_bounds(&audio_only), None);
        assert_eq!(vision_token_bounds(&json!({})), None);
    }

    #[test]
    fn the_routing_projection_keeps_exactly_the_vision_keys_the_bounds_read() {
        assert!(is_vision_bound_key("clip.vision.patch_size", Some("lfm2")));
        assert!(is_vision_bound_key(
            "glmocr.vision.max_pixels",
            Some("glmocr")
        ));
        assert!(is_vision_bound_key(
            "gemma3.mm.tokens_per_image",
            Some("gemma3")
        ));
        assert!(!is_vision_bound_key(
            "clip.audio.num_mel_bins",
            Some("lfm2")
        ));
        assert!(!is_vision_bound_key(
            "other.vision.patch_size",
            Some("lfm2")
        ));
        assert!(!is_vision_bound_key("lfm2.block_count", Some("lfm2")));
    }

    fn vision_model(min: Option<u64>, max: Option<u64>, window: u64) -> CatalogModel {
        let (_, mut model, _) = request("x");
        model.advertised_context = Some(window);
        model.vision_tokens = Some(VisionTokenBounds {
            min_per_image: min,
            max_per_image: max,
        });
        model
    }

    fn image_request(images: usize, text: &str) -> (TaskInput, RouteDecision) {
        let (mut input, _, decision) = request(text);
        input.images = Some(vec!["aW1hZ2U=".into(); images]);
        (input, decision)
    }

    #[test]
    fn images_that_certainly_exceed_the_context_are_refused_before_routing() {
        let model = vision_model(Some(2_000), Some(3_000), 4_096);
        let (input, mut decision) = image_request(3, "describe");
        let error = size_context(&input, &mut decision, &model).unwrap_err();
        assert!(error.body.error.contains("3 image(s)"), "{error:?}");
        // One image fits: the receipt reports the declared range and does not claim a fit.
        let (input, mut decision) = image_request(1, "describe");
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(receipt["mode"], "profile");
        assert_eq!(receipt["images"], 1);
        assert_eq!(receipt["estimated_input_tokens_min"], 2_000 + 3);
        assert_eq!(receipt["estimated_input_tokens_max"], 3_000 + 3);
        assert_eq!(receipt["total_fit_verified"], false);
        assert!(receipt["estimated_input_tokens"].is_null());
    }

    #[test]
    fn text_parts_of_multimodal_messages_are_counted_with_the_declared_image_minimum() {
        let model = vision_model(Some(100), None, 4_096);
        let (mut input, mut decision) = image_request(0, "");
        input.images = None;
        input.prompt = None;
        input.messages = vec![json!({"role":"user","content":[
            {"type":"text","text":"a".repeat(3 * 4_000)},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,".to_owned() + &"A".repeat(500_000)}},
        ]})];
        // About 4,000 text tokens plus one image's minimum exceed 4,096 although the encoded
        // image is not counted as text.
        assert!(size_context(&input, &mut decision, &model).is_err());
        input.messages = vec![json!({"role":"user","content":[
            {"type":"text","text":"short"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,".to_owned() + &"A".repeat(500_000)}},
        ]})];
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(receipt["images"], 1);
        assert!(receipt["estimated_text_input_tokens"].as_u64().unwrap() < 100);
        assert!(receipt["estimated_input_tokens_max"].is_null());
    }

    #[test]
    fn audio_and_models_without_bounds_keep_the_honest_unknown_profile() {
        let unknown = |receipt: Value| {
            assert_eq!(receipt["mode"], "profile");
            assert_eq!(
                receipt["reason"],
                "modality_requires_model_specific_estimator"
            );
        };
        let bounded = vision_model(Some(2_000), Some(3_000), 4_096);
        let (mut input, mut decision) = image_request(1, "describe");
        input.messages = vec![json!({"role":"user","content":"listen","audio":"UklGRg=="})];
        unknown(size_context(&input, &mut decision, &bounded).unwrap());
        let mut undeclared = bounded.clone();
        undeclared.vision_tokens = None;
        let (input, mut decision) = image_request(50, "describe");
        unknown(size_context(&input, &mut decision, &undeclared).unwrap());
    }

    #[test]
    fn a_caller_output_cap_must_fit_beside_the_declared_image_minimum() {
        let model = vision_model(Some(3_000), None, 4_096);
        let (input, mut decision) = image_request(1, "describe");
        decision.options["num_predict"] = json!(1_000);
        assert!(size_context(&input, &mut decision, &model).is_err());
        decision.options["num_predict"] = json!(500);
        assert!(size_context(&input, &mut decision, &model).is_ok());
    }

    #[test]
    fn unicode_estimate_and_non_bucket_model_limit_are_respected() {
        assert!(estimated_text_tokens(&"語".repeat(100)) > estimated_text_tokens(&"a".repeat(100)));
        let (input, mut model, mut decision) = request(&"a".repeat(4000));
        model.advertised_context = Some(3000);
        size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(decision.options["num_ctx"], 3000);
    }

    #[test]
    fn scoped_structured_content_has_no_total_estimate_or_automatic_sizing() {
        let (mut input, model, mut decision) = request("hello");
        input.scope_id = Some("scope".into());
        input.messages =
            vec![json!({"role":"user","content":[{"type":"image","data":"a".repeat(20_000)}]})];
        assert!(size_context(&input, &mut decision, &model).is_err());
        input.route.context_tokens = Some(4096);
        decision.options["num_ctx"] = json!(4096);
        let original = input.messages.clone();
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(receipt["total_fit_verified"], false);
        assert!(receipt["estimated_input_tokens"].is_null());
        assert!(receipt["estimated_text_input_tokens"].as_u64().unwrap() < 512);
        assert_eq!(input.messages, original);
    }

    #[test]
    fn scoped_history_without_a_cap_sizes_from_the_expected_output() {
        let (mut input, model, mut decision) = request("hello");
        input.scope_id = Some("scope".into());
        input.messages = vec![json!({"role":"user","content":"hello"})];
        input.route.context_tokens = Some(4096);
        decision.options["num_ctx"] = json!(4096);
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(receipt["output_reserve_tokens"], 512);
        assert!(decision.options.get("num_predict").is_none());
        // A learned need larger than the window gives way instead of refusing the history.
        decision.output_reserve_tokens = 100_000;
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert!(receipt["output_reserve_tokens"].as_u64().unwrap() < 4096);
    }
}
