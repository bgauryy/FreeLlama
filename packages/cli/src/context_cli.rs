use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ExecutionPreference, PlacementEvidence, TaskExecutionArgs, TaskKind};

#[derive(Debug, Subcommand)]
pub enum ScopeAction {
    /// Create history from an optional HTTP-shaped JSON object.
    Create {
        #[arg(long)]
        json: Option<String>,
    },
    /// Read metadata; history requires --include-messages.
    Get {
        #[arg(long)]
        scope_id: Uuid,
        #[arg(long)]
        include_messages: bool,
    },
    /// Copy a revision into an independent scope; optional JSON overrides defaults/limits.
    Fork {
        #[arg(long)]
        scope_id: Uuid,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: Option<String>,
    },
    /// Delete history and invalidate in-flight commits.
    Delete {
        #[arg(long)]
        scope_id: Uuid,
    },
}

fn object_input(input: Option<String>) -> Result<Value> {
    let value = input
        .map_or(Ok(json!({})), |input| serde_json::from_str(&input))
        .context("--json must be a JSON object")?;
    ensure!(value.is_object(), "--json must be a JSON object");
    Ok(value)
}

pub async fn run_scope(endpoint: &str, action: ScopeAction) -> Result<()> {
    match action {
        ScopeAction::Create { json } => {
            super::print_post(endpoint, "/_freellama/v1/scopes", &object_input(json)?).await
        }
        ScopeAction::Get {
            scope_id,
            include_messages,
        } => {
            super::print_get(
                endpoint,
                &format!("/_freellama/v1/scopes/{scope_id}?include_messages={include_messages}"),
            )
            .await
        }
        ScopeAction::Fork {
            scope_id,
            revision,
            json,
        } => {
            let mut body = object_input(json)?;
            ensure!(
                body.get("revision").is_none(),
                "use --revision instead of a revision in --json"
            );
            body["revision"] = json!(revision);
            super::print_post(
                endpoint,
                &format!("/_freellama/v1/scopes/{scope_id}/fork"),
                &body,
            )
            .await
        }
        ScopeAction::Delete { scope_id } => {
            let response = super::authenticate_request(super::cli_client().delete(format!(
                "{}/_freellama/v1/scopes/{scope_id}",
                endpoint.trim_end_matches('/')
            )))?
            .timeout(super::cli_control_timeout())
            .send()
            .await?;
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                println!("{}", json!({"scope_id": scope_id, "deleted": true}));
                Ok(())
            } else {
                super::print_response(response).await
            }
        }
    }
}

#[derive(Debug, Args)]
pub struct WarmArgs {
    #[arg(
        long,
        env = "FREELLAMA_SERVE_ENDPOINT",
        default_value = "http://127.0.0.1:11435"
    )]
    endpoint: String,
    #[arg(long)]
    model: String,
    #[arg(long, value_enum)]
    task: Option<TaskKind>,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    context_tokens: Option<u64>,
    #[arg(long, value_enum)]
    execution_preference: Option<ExecutionPreference>,
    #[arg(long, value_enum)]
    min_placement_evidence: Option<PlacementEvidence>,
    #[command(flatten)]
    execution: TaskExecutionArgs,
}

pub async fn run_warm(args: WarmArgs) -> Result<()> {
    ensure!(
        !matches!(args.task, Some(TaskKind::Embedding)),
        "warm supports chat models; embedding warming is unsupported"
    );
    let mut body = json!({"model": args.model, "context_tokens": args.context_tokens,
        "execution_preference": args.execution_preference, "min_placement_evidence": args.min_placement_evidence,
        "keep_alive": args.execution.keep_alive, "priority": args.execution.priority,
        "defer": args.execution.defer, "max_wait_seconds": args.execution.max_wait_seconds,
        "timeout_seconds": args.execution.timeout_seconds});
    if let Some(task) = args.task {
        body["task"] = json!(task);
    }
    super::print_post(&args.endpoint, "/_freellama/v1/warm", &body).await
}
