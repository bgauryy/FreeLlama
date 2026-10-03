//! Prompt-aware sizing for managed text requests. Estimates are never tokenizer proof.

use serde_json::{Value, json};

use super::{ApiError, CatalogModel, RouteDecision, TaskInput, TaskKind};

const AUTO_CONTEXT_LIMIT: u64 = 32_768;
const TEMPLATE_MARGIN: u64 = 512;

/// Use UTF-8 bytes so non-ASCII inputs cannot receive the same estimate as equal-length ASCII.
/// Three bytes/token is a conservative heuristic for ordinary text/code, not an exact bound.
pub(super) fn estimated_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
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

/// Project only the execution estimate; the caller's payload and stored history stay intact.
/// Encoded media and structured content require a model-specific tokenizer, not byte counting.
fn scoped_text_payload(input: &TaskInput) -> String {
    let messages: Vec<Value> = input
        .messages
        .iter()
        .map(|message| {
            let fields = message.as_object().expect("scope messages are objects");
            Value::Object(
                fields
                    .iter()
                    .filter(|(key, value)| {
                        !matches!(key.as_str(), "images" | "audio")
                            && (key.as_str() != "content" || value.is_string())
                    })
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        })
        .collect();
    serde_json::to_string(&messages).expect("messages serialize")
}

fn size_scoped_explicit_context(
    input: &TaskInput,
    decision: &RouteDecision,
    unestimated_modality: bool,
) -> Result<Value, ApiError> {
    let payload = if unestimated_modality {
        scoped_text_payload(input)
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
    let output = decision.options["num_predict"].as_u64().ok_or_else(|| {
        ApiError::bad_request("scoped history requires a finite positive output budget")
    })?;
    let required = estimated
        .saturating_add(output)
        .saturating_add(TEMPLATE_MARGIN);
    let available = decision.options["num_ctx"].as_u64().unwrap_or_default();
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
    if matches!(input.operation, super::warming::ManagedOperation::Warm) {
        return Ok(json!({"mode":"warm_profile","tokens":decision.options["num_ctx"]}));
    }
    let unestimated_modality = has_unestimated_modality(input);
    if input.scope_id.is_some() {
        if decision.options["num_predict"]
            .as_u64()
            .is_none_or(|output| output == 0)
        {
            return Err(ApiError::bad_request(
                "scoped history requires a finite positive output budget",
            ));
        }
        if unestimated_modality && input.route.context_tokens.is_none() {
            return Err(ApiError::bad_request(
                "scoped multimodal history requires explicit context_tokens; total token cost requires a model-specific estimator",
            ));
        }
    }
    if input.scope_id.is_some() && input.route.context_tokens.is_some() {
        return size_scoped_explicit_context(input, decision, unestimated_modality);
    }
    if input.route.context_tokens.is_some() {
        return Ok(json!({"mode": "explicit", "tokens": decision.options["num_ctx"]}));
    }
    // Image/audio token cost is model-dependent. Base64 bytes are not text tokens.
    if input.scope_id.is_none() && unestimated_modality {
        return Ok(
            json!({"mode": "profile", "reason": "modality_requires_model_specific_estimator"}),
        );
    }
    let Some(output) = decision.options.get("num_predict").and_then(Value::as_u64) else {
        if input.scope_id.is_some() {
            return Err(ApiError::bad_request(
                "scoped history requires a finite positive output budget",
            ));
        }
        return Ok(json!({"mode": "profile", "reason": "output_budget_not_bounded"}));
    };
    let payload = if input.messages.is_empty() {
        input.prompt.clone().unwrap_or_default()
    } else {
        serde_json::to_string(&input.messages).expect("JSON messages serialize")
    };
    let mut estimated = estimated_text_tokens(&payload);
    for schema in [input.tools.as_ref(), input.request_options.format.as_ref()]
        .into_iter()
        .flatten()
    {
        estimated = estimated.saturating_add(estimated_text_tokens(&schema.to_string()));
    }
    let required = estimated
        .saturating_add(output)
        .saturating_add(TEMPLATE_MARGIN);
    let cap = model
        .advertised_context
        .unwrap_or_else(|| decision.options["num_ctx"].as_u64().unwrap_or(2048))
        .min(AUTO_CONTEXT_LIMIT);
    if required > cap {
        return Err(ApiError::bad_request(format!(
            "estimated prompt plus output reserve requires {required} context tokens, above the automatic cap {cap}; compact the supplied history or set an explicit context_tokens supported by the model"
        )));
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
        "mode": "prompt_estimate", "estimator": "utf8_bytes_div_3",
        "estimated_input_tokens": estimated, "output_reserve_tokens": output,
        "template_margin_tokens": TEMPLATE_MARGIN, "tokens": selected,
        "automatic_cap_tokens": cap, "exact_token_count": false,
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
    fn short_text_uses_smallest_bucket_and_preserves_output_budget() {
        let (input, model, mut decision) = request("hello");
        let receipt = size_context(&input, &mut decision, &model).unwrap();
        assert_eq!(decision.options["num_ctx"], 2048);
        assert_eq!(decision.options["num_predict"], 512);
        assert_eq!(receipt["exact_token_count"], false);
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
    fn scoped_output_budget_must_be_positive_with_automatic_or_explicit_context() {
        let (mut input, model, mut decision) = request("hello");
        input.scope_id = Some("scope".into());
        input.messages = vec![json!({"role":"user","content":"hello"})];
        for context in [None, Some(4096)] {
            input.route.context_tokens = context;
            for output in [0, -1, -2] {
                decision.options["num_predict"] = json!(output);
                assert!(size_context(&input, &mut decision, &model).is_err());
            }
        }
    }
}
