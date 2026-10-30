//! Route and task requests sent to a running `freellama serve`.

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use freellama::{
    capability::Capability,
    platform::{RouteInput, TaskKind},
};
use serde_json::{Value, json};

use super::args::{RouteArgs, TaskArgs};
use super::http::print_post;

pub(crate) async fn request_route(args: RouteArgs) -> Result<()> {
    let route = RouteInput {
        task: args.task,
        objective: args.objective,
        model: args.model,
        session_id: args.session,
        context_tokens: args.context_tokens,
        execution_preference: args.execution_preference,
        min_placement_evidence: args.min_placement_evidence,
        required_capabilities: args
            .required_capabilities
            .iter()
            .map(|s| Capability::parse(s))
            .collect(),
        min_confidence: args.min_confidence,
    };
    print_post(
        &args.endpoint,
        "/_freellama/v1/routes",
        &serde_json::to_value(route)?,
    )
    .await
}

/// Forward caller controls without resolving defaults locally; scopes own omitted routing fields.
pub(crate) async fn request_task(args: TaskArgs) -> Result<()> {
    let TaskArgs {
        prompt,
        endpoint,
        task,
        objective,
        model,
        session,
        context_tokens,
        scope_id,
        scope_revision,
        execution_preference,
        min_placement_evidence,
        images,
        input_file,
        min_confidence,
        required_capabilities,
        execution,
    } = args;
    let mut body = json!({
        "model": model, "session_id": session, "context_tokens": context_tokens,
        "scope_id": scope_id, "scope_revision": scope_revision,
        "min_confidence": min_confidence,
    });
    if let Some(task) = task {
        body["task"] = json!(task);
    }
    if let Some(objective) = objective {
        body["objective"] = json!(objective);
    }
    if let Some(preference) = execution_preference {
        body["execution_preference"] = json!(preference);
    }
    if let Some(evidence) = min_placement_evidence {
        body["min_placement_evidence"] = json!(evidence);
    }
    if !required_capabilities.is_empty() {
        body["required_capabilities"] = json!(required_capabilities);
    }
    body["keep_alive"] = json!(execution.keep_alive);
    body["defer"] = json!(execution.defer);
    body["timeout_seconds"] = json!(execution.timeout_seconds);
    body["priority"] = json!(execution.priority);
    body["max_wait_seconds"] = json!(execution.max_wait_seconds);
    if execution.stop_on_repetition {
        body["stop_on_repetition"] = json!(true);
    }

    // Omitted flags must remain absent so scoped routing defaults can apply.
    body.as_object_mut()
        .expect("task object")
        .retain(|_, value| !value.is_null());

    // Ollama takes images as base64 with no data-URI prefix, attached to the user message.
    // Without this the CLI could select a vision model but never hand it anything to look at —
    // `--task vision` was an advertised capability that could not actually be exercised.
    if !images.is_empty() {
        let encoded: Vec<String> = images
            .iter()
            .map(|path| {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("read image {}", path.display()))?;
                Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
            })
            .collect::<Result<_>>()?;
        body["images"] = json!(encoded);
    }

    let embedding = matches!(task, Some(TaskKind::Embedding));
    if let Some(path) = &input_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read input file {}", path.display()))?;
        let (field, value) = file_input(&text, embedding)
            .with_context(|| format!("input file {}", path.display()))?;
        body[field] = value;
    } else {
        let field = if embedding { "input" } else { "prompt" };
        body[field] = Value::String(prompt.context("a prompt or --input-file is required")?);
    }
    print_post(&endpoint, "/_freellama/v1/tasks", &body).await
}

/// The request field an `--input-file` fills: embedding input takes one item per non-empty line
/// (one batched request is far cheaper than one per line); every other task takes the whole file
/// as its prompt.
fn file_input(text: &str, embedding: bool) -> Result<(&'static str, Value)> {
    if embedding {
        let items: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        ensure!(!items.is_empty(), "has no non-empty lines");
        return Ok(("input", json!(items)));
    }
    ensure!(!text.trim().is_empty(), "is empty");
    Ok(("prompt", Value::String(text.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::file_input;
    use serde_json::json;

    #[test]
    fn an_input_file_is_embedding_items_or_one_prompt() {
        assert_eq!(
            file_input("a\n\n b \n", true).unwrap(),
            ("input", json!(["a", " b "]))
        );
        assert_eq!(
            file_input("line one\nline two\n", false).unwrap(),
            ("prompt", json!("line one\nline two\n"))
        );
        assert!(file_input("\n \n", true).is_err());
        assert!(file_input("  \n", false).is_err());
    }
}
