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

pub(super) fn size_context(
    input: &TaskInput,
    decision: &mut RouteDecision,
    model: &CatalogModel,
) -> Result<Value, ApiError> {
    if input.route.context_tokens.is_some() {
        return Ok(json!({"mode": "explicit", "tokens": decision.options["num_ctx"]}));
    }
    // Image/audio token cost is model-dependent. Base64 bytes are not text tokens.
    if matches!(input.route.task, TaskKind::Embedding | TaskKind::Vision)
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
    {
        return Ok(
            json!({"mode": "profile", "reason": "modality_requires_model_specific_estimator"}),
        );
    }
    let Some(output) = decision.options.get("num_predict").and_then(Value::as_u64) else {
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
}
