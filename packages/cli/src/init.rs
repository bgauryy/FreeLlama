//! `freellama init` and `freellama doctor`: first-run setup and diagnostics.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use freellama::doctor;
use reqwest::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use freellama::serve_client;

pub(crate) fn generate_auth_token(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create token directory {}", parent.display()))?;
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create new authentication token file {}", path.display()))?;
    writeln!(file, "{token}").context("write authentication token")?;
    file.sync_all().context("sync authentication token")?;
    crate::emit(format_args!(
        "Created authentication token file {}",
        path.display()
    ))
}

pub(crate) fn initialization_status(
    ollama_ready: bool,
    serve_ready: bool,
    has_models: bool,
) -> &'static str {
    if !ollama_ready {
        "blocked"
    } else if !serve_ready || !has_models {
        "setup_required"
    } else {
        "ready"
    }
}

/// Side-effect-free first-run receipt. Initialization must discover the real host and inventory
/// before discussing a model, and discovery must never be interpreted as pull permission.
pub(crate) async fn initialize(ollama_endpoint: String, serve_endpoint: String) -> Result<()> {
    let diagnostics = match doctor(&ollama_endpoint).await {
        Ok(value) => json!({"status": "ok", "report": value}),
        Err(error) => json!({"status": "blocked", "error": error.to_string()}),
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build init HTTP client")?;
    let tags = match client
        .get(freellama::endpoint_url(&ollama_endpoint, "/api/tags"))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response
            .json::<Value>()
            .await
            .unwrap_or_else(|error| json!({"error": error.to_string()})),
        Ok(response) => json!({"error": format!("HTTP {}", response.status())}),
        Err(error) => json!({"error": error.to_string()}),
    };
    let health_request = client.get(freellama::endpoint_url(
        &serve_endpoint,
        "/_freellama/v1/health",
    ));
    let health = match serve_client::authenticate(health_request)?.send().await {
        Ok(response) if response.status().is_success() => {
            response.json::<Value>().await.unwrap_or_else(
                |error| json!({"status": "stale_or_invalid", "error": error.to_string()}),
            )
        }
        Ok(response) => json!({"status": "unavailable", "http_status": response.status().as_u16()}),
        Err(error) => json!({"status": "unavailable", "error": error.to_string()}),
    };
    let installed_models = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    let ollama_ready = diagnostics["status"] == "ok" && tags.get("error").is_none();
    let serve_ready = health["status"] == "ok";
    let mut next_steps = Vec::new();
    if !ollama_ready {
        next_steps.push(json!({
            "action": "install_or_start_ollama",
            "instruction": "Install Ollama from https://ollama.com/download, or start `ollama serve`, then rerun `freellama init`."
        }));
    } else if installed_models.is_empty() {
        next_steps.push(json!({
            "action": "choose_model",
            "instruction": "Describe the workload, modality, quality, context, download, disk, and memory constraints. Inspect exact tags and ask approval before `ollama pull`."
        }));
    }
    if ollama_ready && !serve_ready {
        next_steps.push(json!({
            "action": "start_freellama",
            "instruction": format!("Start `freellama serve --upstream {ollama_endpoint}`; add a second loopback Ollama and exact --cpu-model tags only after placement trials.")
        }));
    }
    if serve_ready && !installed_models.is_empty() {
        next_steps.push(json!({
            "action": "verify_managed_task",
            "instruction": "Preview with `freellama route --objective fastest`, run one bounded task with configured placement evidence, inspect execution.observation, then require observed evidence where placement matters."
        }));
        next_steps.push(json!({
            "action": "configure_mcp",
            "instruction": "Build packages/mcp, launch packages/mcp/dist/index.js over stdio, call doctor, then list installed/resident models before delegating."
        }));
    }
    crate::emit(serde_json::to_string_pretty(&json!({
            "status": initialization_status(ollama_ready, serve_ready, !installed_models.is_empty()),
            "readiness": {"ollama":ollama_ready,"managed_service":serve_ready,"installed_models":!installed_models.is_empty()},
            "ollama_endpoint": ollama_endpoint,
            "serve_endpoint": serve_endpoint,
            "diagnostics": diagnostics,
            "installed_models": installed_models,
            "serve_health": health,
            "next_steps": next_steps,
            "side_effects_performed": false,
            "model_pull_requires_exact_tag_approval": true
    }))?)
}

pub(crate) async fn print_doctor(endpoint: &str) -> Result<()> {
    crate::emit(serde_json::to_string_pretty(&doctor(endpoint).await?)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::generate_auth_token;

    #[test]
    fn auth_token_file_is_created_owner_only_and_readable_by_serve() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("freellama-auth-{}", uuid::Uuid::new_v4().simple()));
        let path = directory.join("nested").join("token");
        generate_auth_token(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let result = freellama::auth::read_token_file(&path);
        let again = generate_auth_token(&path);
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        assert!(result.is_ok(), "{result:?}");
        assert!(
            again.is_err(),
            "an existing token file must not be overwritten"
        );
    }
}
