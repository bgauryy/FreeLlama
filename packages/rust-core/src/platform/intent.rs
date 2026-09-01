//! Natural-language route intent: the schema the local interpreter model must emit, strict
//! parsing of its structured output, and the deterministic guard layer (`normalize_route_intent`)
//! that overrides the model on explicit, high-impact phrases. Depends on the routing types in
//! `super`, never the other way round.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model_bench::Capability;

use super::{Objective, RouteInput, TaskKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteIntent {
    pub task: TaskKind,
    pub objective: Objective,
    pub context_tokens: Option<u64>,
    pub requires_tools: bool,
    pub requires_vision: bool,
}

impl RouteIntent {
    #[must_use]
    pub fn into_route_input(self, session_id: Option<String>) -> RouteInput {
        let mut required_capabilities = BTreeSet::new();
        if self.requires_tools {
            required_capabilities.insert(Capability::Tools);
        }
        if self.requires_vision {
            required_capabilities.insert(Capability::Vision);
        }
        RouteInput {
            task: self.task,
            objective: self.objective,
            session_id,
            required_capabilities,
            context_tokens: self.context_tokens,
            ..RouteInput::default()
        }
    }
}

/// Return the strict schema used by the local natural-language intent model.
#[must_use]
pub fn intent_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "task": {"type": "string", "enum": ["completion", "coding", "code_repair", "tools", "browser", "vision", "embedding", "long_context"]},
            "objective": {"type": "string", "enum": ["fastest", "balanced", "quality"]},
            "context_tokens": {"type": ["integer", "null"], "minimum": 512},
            "requires_tools": {"type": "boolean"},
            "requires_vision": {"type": "boolean"}
        },
        "required": ["task", "objective", "context_tokens", "requires_tools", "requires_vision"]
    })
}

/// Parse and validate the local intent model's structured output.
///
/// # Errors
///
/// Returns an error for malformed JSON, unknown fields, or invalid enum values.
pub fn parse_route_intent(content: &str) -> Result<RouteIntent> {
    serde_json::from_str(content).context("parse structured route intent")
}

/// Apply deterministic guards for explicit, high-impact natural-language constraints.
#[must_use]
pub fn normalize_route_intent(mut intent: RouteIntent, text: &str) -> (RouteIntent, Vec<String>) {
    let text = text.to_ascii_lowercase();
    let mut adjustments = Vec::new();
    let has_vision_evidence = contains_any(&text, &["screenshot", "image", "photo", "vision"]);
    let has_tool_evidence = contains_any(
        &text,
        &[
            "click",
            "tool call",
            "function call",
            "use tools",
            "search the repository",
            "edit files",
        ],
    );
    let has_context_evidence = contains_any(
        &text,
        &[
            "context",
            "token",
            "large document",
            "long document",
            "long input",
        ],
    );
    if let Some((task, reason)) = explicit_task(&text)
        && intent.task != task
    {
        intent.task = task;
        adjustments.push(reason.to_owned());
    }

    if contains_any(
        &text,
        &[
            "maximum quality",
            "maximum answer quality",
            "best quality",
            "highest quality",
            "best evaluated",
        ],
    ) && intent.objective != Objective::Quality
    {
        intent.objective = Objective::Quality;
        adjustments.push("explicit_quality_objective".to_owned());
    } else if contains_any(
        &text,
        &[
            "fast as possible",
            "fastest",
            "lowest latency",
            "low latency",
        ],
    ) && intent.objective != Objective::Fastest
    {
        intent.objective = Objective::Fastest;
        adjustments.push("explicit_speed_objective".to_owned());
    }
    normalize_inferred_requirements(
        &mut intent,
        has_vision_evidence,
        has_tool_evidence,
        has_context_evidence,
        &mut adjustments,
    );
    (intent, adjustments)
}

fn explicit_task(text: &str) -> Option<(TaskKind, &'static str)> {
    if contains_any(text, &["semantic vector", "embedding", "vector embedding"]) {
        Some((TaskKind::Embedding, "explicit_embedding_term"))
    } else if contains_any(
        text,
        &[
            "fix the bug",
            "bug fix",
            "repair the code",
            "implement the fix",
            "edit files",
        ],
    ) {
        Some((TaskKind::CodeRepair, "explicit_code_repair_term"))
    } else if contains_any(
        text,
        &[
            "browser",
            "webpage",
            "web page",
            "checkout page",
            "click the",
        ],
    ) {
        Some((TaskKind::Browser, "explicit_browser_term"))
    } else if contains_any(
        text,
        &[
            "code review",
            "codebase",
            "debug",
            "rust code",
            "write code",
        ],
    ) {
        Some((TaskKind::Coding, "explicit_coding_term"))
    } else {
        None
    }
}

fn normalize_inferred_requirements(
    intent: &mut RouteIntent,
    has_vision_evidence: bool,
    has_tool_evidence: bool,
    has_context_evidence: bool,
    adjustments: &mut Vec<String>,
) {
    if has_vision_evidence && !intent.requires_vision {
        intent.requires_vision = true;
        adjustments.push("explicit_vision_requirement".to_owned());
    }
    if (matches!(
        intent.task,
        TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair
    ) || has_tool_evidence)
        && !intent.requires_tools
    {
        intent.requires_tools = true;
        adjustments.push("explicit_tool_requirement".to_owned());
    }
    if matches!(intent.task, TaskKind::Embedding) {
        if intent.requires_tools {
            intent.requires_tools = false;
            adjustments.push("embedding_clears_tool_requirement".to_owned());
        }
        if intent.requires_vision {
            intent.requires_vision = false;
            adjustments.push("embedding_clears_vision_requirement".to_owned());
        }
    } else {
        if matches!(intent.task, TaskKind::Vision) && !intent.requires_vision {
            intent.requires_vision = true;
            adjustments.push("vision_task_requires_vision".to_owned());
        } else if !matches!(intent.task, TaskKind::Vision)
            && !has_vision_evidence
            && intent.requires_vision
        {
            intent.requires_vision = false;
            adjustments.push("unsupported_vision_requirement_cleared".to_owned());
        }
        if !matches!(
            intent.task,
            TaskKind::Browser | TaskKind::Tools | TaskKind::CodeRepair
        ) && !has_tool_evidence
            && intent.requires_tools
        {
            intent.requires_tools = false;
            adjustments.push("unsupported_tool_requirement_cleared".to_owned());
        }
    }
    if intent.context_tokens.is_some() && !has_context_evidence {
        intent.context_tokens = None;
        adjustments.push("unsupported_context_requirement_cleared".to_owned());
    } else if intent.context_tokens.is_some_and(|tokens| tokens < 512) {
        intent.context_tokens = Some(512);
        adjustments.push("context_requirement_clamped".to_owned());
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| {
        text.match_indices(needle).any(|(start, matched)| {
            let mut end = start + matched.len();
            let starts_at_boundary = text[..start]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_alphanumeric());
            // Intent terms are written in their singular form, but ordinary plural forms should
            // still count. Only consume a trailing `s` when it is itself at a word boundary, so
            // `photos` matches `photo` while `photosynthesis` remains unrelated.
            if text[end..].starts_with('s')
                && text[end + 1..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_alphanumeric())
            {
                end += 1;
            }
            let ends_at_boundary = text[end..]
                .chars()
                .next()
                .is_none_or(|character| !character.is_alphanumeric());
            starts_at_boundary && ends_at_boundary
        })
    })
}

pub(super) fn intent_system_prompt() -> &'static str {
    "Translate the user's natural-language request into the route-intent schema. Use browser for webpage navigation or interaction; code_repair for fixing a repository bug or editing files to implement a repair; coding for code review, explanation, or diagnosis without a requested repair; tools when function calls are required; vision for image-only analysis; embedding for vectors or semantic search; long_context only for explicitly large documents; otherwise completion. Use fastest only when the user explicitly prioritizes speed or latency, quality only when they explicitly prioritize maximum answer quality, and balanced otherwise. Set requires_tools and requires_vision independently. Never choose or name a model."
}
