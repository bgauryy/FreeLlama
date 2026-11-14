//! Route and task requests sent to a running `freellama serve`.

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use freellama::{
    capability::Capability,
    platform::{RouteInput, TaskKind},
};
use serde_json::{Value, json};

use super::args::{RouteArgs, TaskArgs};
use super::http::{post_json, print_post, try_get_json};

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
        questions,
        nearest,
        top,
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
    if let Some(stop) = execution.stop_on_repetition {
        body["stop_on_repetition"] = json!(stop);
    }
    if !execution.fallback_models.is_empty() {
        body["fallback_models"] = json!(execution.fallback_models);
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

    if matches!(task, Some(TaskKind::Decision)) {
        let questions = questions.context("--task decision requires --questions")?;
        return request_decision(&endpoint, body, &questions, input_file.as_deref(), prompt).await;
    }
    ensure!(
        questions.is_none(),
        "--questions applies to --task decision only"
    );
    let embedding = matches!(task, Some(TaskKind::Embedding));
    ensure!(
        nearest.is_none() || embedding,
        "--nearest applies to --task embedding only"
    );
    if let (Some(query), Some(path)) = (&nearest, &input_file) {
        return request_nearest(&endpoint, body, query, path, top).await;
    }
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

/// `catalog`: the shipped picks, annotated with what serve reports as installed and available.
/// With serve unreachable, `installed` and `fits_now` stay null: unknown is not "no".
pub(crate) async fn request_catalog(endpoint: &str, job: Option<&str>) -> Result<()> {
    let models = try_get_json(endpoint, "/_freellama/v1/models").await;
    let status = try_get_json(endpoint, "/_freellama/v1/status").await;
    let installed: Option<Vec<String>> = models.as_ref().map(|models| {
        models["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| model["name"].as_str().map(str::to_owned))
            .collect()
    });
    let available = status
        .as_ref()
        .and_then(|status| status["host"]["available_memory_bytes"].as_u64());
    let view = catalog_view(job, installed.as_deref(), available)?;
    crate::emit(serde_json::to_string_pretty(&view)?)
}

fn catalog_view(
    job: Option<&str>,
    installed: Option<&[String]>,
    available: Option<u64>,
) -> Result<Value> {
    let mut catalog: Value =
        serde_json::from_str(freellama::MODEL_CATALOG).context("shipped model catalog")?;
    let jobs = catalog["jobs"].as_array_mut().context("catalog jobs")?;
    if let Some(job) = job {
        let known: Vec<String> = jobs
            .iter()
            .filter_map(|entry| entry["job"].as_str().map(str::to_owned))
            .collect();
        ensure!(
            known.iter().any(|name| name == job),
            "unknown job {job:?}; choose one of {}",
            known.join(", ")
        );
        jobs.retain(|entry| entry["job"] == job);
    }
    for entry in jobs.iter_mut() {
        entry["cookbook"] = json!(format!(
            "docs/{}.md",
            entry["cookbook"].as_str().unwrap_or_default()
        ));
        for pick in entry["picks"].as_array_mut().context("catalog picks")? {
            let tag = pick["tag"].as_str().unwrap_or_default().to_owned();
            pick["installed"] = installed.map_or(Value::Null, |names| json!(names.contains(&tag)));
            let needed = pick["memory_gb"].as_f64();
            pick["fits_now"] = match (available, needed) {
                // Memory sizes fit f64 exactly enough for a go/no-go comparison.
                #[expect(clippy::cast_precision_loss, reason = "a go/no-go comparison in GB")]
                (Some(bytes), Some(gb)) => json!(bytes as f64 >= gb * 1e9),
                _ => Value::Null,
            };
        }
    }
    Ok(catalog)
}

/// `--task decision`: the prompt or `--input-file` is the state (JSON stays JSON).
async fn request_decision(
    endpoint: &str,
    mut body: Value,
    questions: &str,
    input_file: Option<&std::path::Path>,
    prompt: Option<String>,
) -> Result<()> {
    body["questions"] = decision_questions(questions)?;
    let state = match input_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read input file {}", path.display()))?,
        None => prompt.context("a state (the prompt) or --input-file is required")?,
    };
    ensure!(!state.trim().is_empty(), "the decision state is empty");
    // JSON state keeps its fields, which questions may name in backticks.
    body["state"] = serde_json::from_str::<Value>(&state)
        .ok()
        .filter(|value| value.is_object() || value.is_array())
        .unwrap_or(Value::String(state));
    print_post(endpoint, "/_freellama/v1/tasks", &body).await
}

/// `--task embedding --nearest`: embed the query with the file's lines and print the closest.
async fn request_nearest(
    endpoint: &str,
    mut body: Value,
    query: &str,
    path: &std::path::Path,
    top: usize,
) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read input file {}", path.display()))?;
    let items: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    ensure!(
        !items.is_empty(),
        "input file {} has no non-empty lines",
        path.display()
    );
    body["input"] = json!(items);
    // The core embeds the query with the lines and ranks them.
    body["nearest"] = json!({ "query": query, "top": top });
    let result = post_json(endpoint, "/_freellama/v1/tasks", &body).await?;
    let matches = result["nearest"]["matches"]
        .as_array()
        .context("the embedding response carried no ranking")?;
    let ranked: Vec<Value> = matches
        .iter()
        .map(|found| {
            let index = found["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok());
            json!({
                "item": index.map(|index| index + 1),
                "score": found["score"],
                "text": index.and_then(|index| items.get(index).copied()).unwrap_or_default(),
            })
        })
        .collect();
    crate::emit(serde_json::to_string_pretty(&json!({
        "selected_model": result["route"]["selected_model"],
        "nearest": ranked,
    }))?)
}

/// `--questions` as a JSON object, inline or from `@path`.
fn decision_questions(argument: &str) -> Result<Value> {
    let text = match argument.strip_prefix('@') {
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("read questions file {path}"))?
        }
        None => argument.to_owned(),
    };
    let questions: Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("--questions is not valid JSON: {error}"))?;
    ensure!(
        questions
            .as_object()
            .is_some_and(|object| !object.is_empty()),
        "--questions must be a JSON object of named questions"
    );
    Ok(questions)
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
    use super::{catalog_view, decision_questions, file_input};

    #[test]
    fn the_catalog_marks_what_is_installed_and_fits_and_stays_unknown_without_serve() {
        let names = ["embeddinggemma-2:740m-q8_0".to_owned()];
        let view = catalog_view(Some("find-code"), Some(&names), Some(2_000_000_000)).unwrap();
        let picks = view["jobs"][0]["picks"].as_array().unwrap();
        assert_eq!(picks[0]["tag"], "embeddinggemma-2:740m-q8_0");
        assert_eq!(picks[0]["installed"], true);
        assert_eq!(picks[0]["fits_now"], true);
        assert_eq!(picks[1]["installed"], false);
        assert_eq!(view["jobs"][0]["cookbook"], "docs/cookbooks/find-code.md");
        let unknown = catalog_view(Some("ocr"), None, None).unwrap();
        assert!(unknown["jobs"][0]["picks"][0]["installed"].is_null());
        assert!(unknown["jobs"][0]["picks"][1]["fits_now"].is_null());
        assert!(catalog_view(Some("poetry"), None, None).is_err());
    }

    use serde_json::json;

    #[test]
    fn decision_questions_are_one_json_object() {
        let inline = r#"{"kind":{"type":"noul","instructions":"Broken?"}}"#;
        assert_eq!(
            decision_questions(inline).unwrap()["kind"]["type"],
            json!("noul")
        );
        assert!(decision_questions("[1]").is_err());
        assert!(decision_questions("{}").is_err());
        assert!(decision_questions("not json").is_err());
        let message = decision_questions("{bad").unwrap_err().to_string();
        assert!(
            message.starts_with("--questions is not valid JSON: key must be a string"),
            "{message}"
        );
        assert!(decision_questions("@/nonexistent/questions.json").is_err());
    }

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
