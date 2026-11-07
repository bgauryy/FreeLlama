//! HTTP plumbing for talking to a running `freellama serve`: clients, timeouts, auth and output.

use anyhow::{Context, Result};
use freellama::serve_client;
use serde_json::Value;

/// Turn a transport failure into an actionable message.
///
/// Every control-plane subcommand needs a running `freellama serve`, and the raw reqwest error
/// ("client error (Connect)") does not say so. Naming the missing process and the command that
/// starts it is the difference between a dead end and a next step.
fn explain_transport(endpoint: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_connect() || error.is_timeout() {
        return anyhow::anyhow!(
            "cannot reach the FreeLlama control plane at {endpoint}.\n\
             Start it with:\n  freellama serve\n\
             (`doctor` is the one subcommand that works without it.)\n\
             Underlying error: {error}"
        );
    }
    error.into()
}

/// Print a JSON response, or fail with the server's own explanation of why it refused.
///
/// `error_for_status()` discards the body, which is where every useful refusal lives — a
/// `min_confidence` refusal names the grade, the evidence, the model it would have picked and the
/// two commands that raise the grade, and all of that was being replaced by a bare
/// "422 Unprocessable Entity". Surface the server's own explanation instead.
///
/// A success whose body is not JSON is an error, not silence: printing nothing and exiting 0 would
/// tell the caller the command worked when nothing was reported.
pub(crate) async fn print_response(response: reqwest::Response) -> Result<()> {
    let value = response_json(response).await?;
    crate::emit(serde_json::to_string_pretty(&value)?)
}

/// The JSON body of a successful response, or the server's own refusal as the error.
async fn response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let body = response.text().await.context("read response body")?;
    let payload = serde_json::from_str::<Value>(&body).ok();
    if !status.is_success() {
        let detail = payload
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
            .map_or_else(
                || {
                    if body.trim().is_empty() {
                        status.to_string()
                    } else {
                        body.trim().to_owned()
                    }
                },
                ToOwned::to_owned,
            );
        anyhow::bail!("{detail}");
    }
    payload.with_context(|| {
        format!(
            "{status} response from the platform was not JSON: {}",
            body.trim()
        )
    })
}

/// GET a JSON document, or `None` when the server cannot be reached or refuses.
pub(crate) async fn try_get_json(endpoint: &str, path: &str) -> Option<Value> {
    let request = serve_client::client().get(freellama::endpoint_url(endpoint, path));
    let response = serve_client::authenticate(request)
        .ok()?
        .timeout(serve_client::control_timeout())
        .send()
        .await
        .ok()?;
    response_json(response).await.ok()
}

pub(crate) async fn print_get(endpoint: &str, path: &str) -> Result<()> {
    let request = serve_client::client().get(freellama::endpoint_url(endpoint, path));
    let response = serve_client::authenticate(request)?
        .timeout(serve_client::control_timeout())
        .send()
        .await
        .map_err(|e| explain_transport(endpoint, e))?;
    print_response(response).await
}

pub(crate) async fn print_post(endpoint: &str, path: &str, body: &Value) -> Result<()> {
    let value = post_json(endpoint, path, body).await?;
    crate::emit(serde_json::to_string_pretty(&value)?)
}

/// POST `body` and return the JSON result, for commands that reshape it before printing.
pub(crate) async fn post_json(endpoint: &str, path: &str, body: &Value) -> Result<Value> {
    // `/tasks` and `/warm` run a model; everything else is a decision.
    let timeout = if path.ends_with("/tasks") || path.ends_with("/warm") {
        serve_client::task_timeout()
    } else {
        serve_client::control_timeout()
    };
    let request = serve_client::client()
        .post(freellama::endpoint_url(endpoint, path))
        .timeout(timeout)
        .json(body);
    let response = serve_client::authenticate(request)?
        .send()
        .await
        .map_err(|e| explain_transport(endpoint, e))?;
    response_json(response).await
}
