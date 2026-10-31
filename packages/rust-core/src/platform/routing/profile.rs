//! The request profile (options, output reserve, thinking) chosen for one routed model.
use super::{Capability, CatalogModel, ModelRequestDefaults, RouteInput, TaskKind, Value, json};

/// Request options and validation mode chosen for one routed model.
pub(super) struct TaskProfile {
    pub(super) name: String,
    pub(super) options: Value,
    pub(super) output_reserve: u64,
    pub(super) think: Value,
    pub(super) strict_tool_validation: bool,
    /// True when a policy `model_options` entry changed the generic task profile.
    pub(super) policy_model_options: bool,
    /// True when the policy asked for `think = true` on a model without the Thinking capability.
    pub(super) policy_think_dropped: bool,
}

/// Expected room for a thinking-capable model's reasoning plus an answer, matching the coding
/// estimate; used only to size the context, and its own runs can only raise it.
pub(super) const THINKING_OUTPUT_ESTIMATE: u64 = 2_048;

pub(super) fn profile(input: &RouteInput, model: &CatalogModel) -> TaskProfile {
    let policy = model.policy_options.get(&input.task).copied();
    let requested_context = requested_context(input);
    let num_ctx = requested_context
        .min(model.advertised_context.unwrap_or(requested_context))
        .min(u64::from(u32::MAX));
    // How much output to expect, only for sizing the context window: a starting estimate per task,
    // raised by what this model revision has needed before (plus a quarter for variation) and, for
    // a model reporting the thinking capability, by room to reason. Never sent as a cap.
    let (name, starting_estimate, think, strict) = match input.task {
        TaskKind::Browser => ("browser_action", 64, Value::Bool(false), true),
        TaskKind::Tools => ("tools", 256, Value::Bool(false), true),
        TaskKind::CodeRepair => ("code_repair", 2_048, thinking_or_off(model, "medium"), true),
        TaskKind::Coding => ("coding", 2_048, thinking_or_off(model, "medium"), false),
        TaskKind::LongContext => (
            "long_context",
            2_048,
            thinking_or_off(model, "medium"),
            false,
        ),
        TaskKind::Vision => ("vision", 512, Value::Bool(false), false),
        TaskKind::Embedding => ("embedding", 0, Value::Null, false),
        TaskKind::Completion => ("completion", 512, Value::Bool(false), false),
    };
    let output_reserve = if starting_estimate == 0 {
        0
    } else {
        let observed = model
            .learned_output_tokens
            .map_or(0, |tokens| tokens.saturating_add(tokens / 4));
        let thinking = if model.capabilities.contains(&Capability::Thinking) {
            THINKING_OUTPUT_ESTIMATE
        } else {
            0
        };
        // Never more than half the model's own window: the prompt needs the rest.
        let ceiling = model
            .advertised_context
            .map_or(u64::MAX, |context| context / 2);
        starting_estimate.max(observed.max(thinking).min(ceiling))
    };
    // An operator policy may cap output for one model and task; nothing else does.
    let policy_cap = policy.and_then(|entry| entry.num_predict);
    // `think = true` only reaches a model that can think; Ollama rejects it otherwise.
    let policy_think = policy.and_then(|entry| entry.think);
    let policy_think_dropped =
        policy_think == Some(true) && !model.capabilities.contains(&Capability::Thinking);
    let think = match policy_think {
        Some(true) => thinking_or_off(model, true),
        Some(false) => Value::Bool(false),
        None => think,
    };
    let mut options = json!({"num_ctx": num_ctx});
    if let Some(cap) = policy_cap {
        options["num_predict"] = json!(cap);
    }
    TaskProfile {
        name: name.to_owned(),
        options,
        output_reserve: policy_cap.unwrap_or(output_reserve),
        think,
        strict_tool_validation: strict,
        policy_model_options: policy.is_some_and(|entry| entry != ModelRequestDefaults::default()),
        policy_think_dropped,
    }
}

pub(super) fn requested_context(input: &RouteInput) -> u64 {
    input.context_tokens.unwrap_or(match input.task {
        TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair => 8_192,
        TaskKind::LongContext => 32_768,
        // Embeddings are not chat: embedding models commonly advertise 2048, so the chat default
        // would make every embed look like a context miss.
        TaskKind::Embedding => 2_048,
        _ => 16_384,
    })
}

pub(super) fn thinking_or_off(model: &CatalogModel, effort: impl Into<Value>) -> Value {
    if model.capabilities.contains(&Capability::Thinking) {
        effort.into()
    } else {
        Value::Bool(false)
    }
}
