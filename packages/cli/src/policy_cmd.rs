//! `freellama policy from-eval`: the I/O around `freellama::platform::policy_eval`.

use std::{collections::BTreeMap, io::Write as _, time::Duration};

use anyhow::{Context, Result};
use freellama::platform::policy_eval::{is_smoke, qualify, render_policy};
use serde_json::Value;

use crate::{PlainError, args::PolicyFromEvalArgs};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `policy from-eval`. Failures surface as one `error: …` line with exit status 1.
pub(crate) async fn from_eval(args: PolicyFromEvalArgs) -> Result<()> {
    let rendered = generate(&args)
        .await
        .map_err(|error| PlainError(format!("{error:#}")))?;
    if let Some(out) = &args.out {
        std::fs::write(out, rendered)
            .with_context(|| format!("write {}", out.display()))
            .map_err(|error| PlainError(format!("{error:#}")))?;
        crate::emit(format_args!("wrote {}", out.display()))?;
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(rendered.as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

async fn generate(args: &PolicyFromEvalArgs) -> Result<String> {
    let source = args.aggregate.display().to_string();
    let text = std::fs::read_to_string(&args.aggregate)
        .with_context(|| format!("read aggregate {source}"))?;
    let aggregate: Value =
        serde_json::from_str(&text).with_context(|| format!("parse aggregate {source}"))?;
    let installed = installed_models(&args.ollama_endpoint).await?;
    let (qualified, benchmark_date) =
        qualify(&aggregate, &installed, args.min_pass, args.allow_smoke)?;
    let smoke = is_smoke(&qualified);
    Ok(render_policy(
        &BTreeMap::from([(args.task, qualified)]),
        &source,
        &benchmark_date,
        args.min_pass,
        smoke,
    ))
}

/// List installed Ollama tags from `<endpoint>/api/tags`.
async fn installed_models(endpoint: &str) -> Result<Vec<String>> {
    let url = freellama::endpoint_url(endpoint, "/api/tags");
    let tags: Value = freellama::build_http_client(|builder| builder.timeout(CONTROL_TIMEOUT))?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("list installed models from {url}"))?
        .error_for_status()
        .with_context(|| format!("list installed models from {url}"))?
        .json()
        .await
        .with_context(|| format!("decode installed models from {url}"))?;
    tags.get("models")
        .and_then(Value::as_array)
        .with_context(|| format!("{url} response has no `models` list"))?
        .iter()
        .map(|model| {
            model
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .with_context(|| format!("{url} lists a model without a `name`"))
        })
        .collect()
}
