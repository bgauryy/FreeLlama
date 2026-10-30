//! Small Ollama HTTP helpers shared by doctor, the model benchmark and the platform.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};

/// Join an endpoint and an absolute path without doubling the slash.
#[must_use]
pub fn url(endpoint: &str, path: &str) -> String {
    format!("{}{path}", endpoint.trim_end_matches('/'))
}

/// Whole milliseconds, saturating at `u64::MAX`.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The `/api/generate` body that asks Ollama to unload `model` now (`keep_alive: 0`). The one
/// unload request shape: [`unload`] sends it directly, and the platform sends it through its
/// retrying upstream client.
pub(crate) fn unload_request(model: &str) -> Value {
    json!({ "model": model, "keep_alive": 0, "stream": false })
}

/// Ask Ollama to unload `model` immediately (`keep_alive: 0`).
pub(crate) async fn unload(client: &Client, endpoint: &str, model: &str) -> Result<()> {
    client
        .post(url(endpoint, "/api/generate"))
        .json(&unload_request(model))
        .send()
        .await
        .with_context(|| format!("request unload for {model}"))?
        .error_for_status()
        .with_context(|| format!("Ollama rejected unload for {model}"))?;
    Ok(())
}

/// `(size, size_vram)` of `model` in `/api/ps`, or `(None, None)` when unknown.
pub(crate) async fn resident(
    client: &Client,
    endpoint: &str,
    model: &str,
) -> (Option<u64>, Option<u64>) {
    let Ok(response) = client.get(url(endpoint, "/api/ps")).send().await else {
        return (None, None);
    };
    let Ok(value) = response.json::<Value>().await else {
        return (None, None);
    };
    find_ps_entry(&value, model).map_or((None, None), |entry| {
        (
            entry.get("size").and_then(Value::as_u64),
            entry.get("size_vram").and_then(Value::as_u64),
        )
    })
}

/// Model name of one `/api/ps` entry: `name`, falling back to `model` only when `name` is absent.
pub(crate) fn ps_entry_name(entry: &Value) -> Option<&str> {
    entry
        .get("name")
        .or_else(|| entry.get("model"))
        .and_then(Value::as_str)
}

/// The `/api/ps` entry whose [`ps_entry_name`] is exactly `model`. The single matcher for every
/// residency lookup, so the benchmark and platform agree on what "loaded" means.
pub(crate) fn find_ps_entry<'a>(ps: &'a Value, model: &str) -> Option<&'a Value> {
    ps.get("models")?
        .as_array()?
        .iter()
        .find(|entry| ps_entry_name(entry) == Some(model))
}

pub(crate) async fn server_version(client: &Client, endpoint: &str) -> Result<String> {
    let value: Value = client
        .get(url(endpoint, "/api/version"))
        .send()
        .await
        .context("connect to Ollama")?
        .error_for_status()
        .context("Ollama version request")?
        .json()
        .await
        .context("decode Ollama version")?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("Ollama response has no version")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_matcher_prefers_name_and_falls_back_to_model() {
        let ps = json!({"models": [
            {"name": "alias:latest", "model": "other:latest", "size": 1},
            {"model": "bare:latest", "size": 2},
        ]});
        assert_eq!(find_ps_entry(&ps, "alias:latest").unwrap()["size"], 1);
        // `model` is only a fallback: an entry with a different `name` is a different runner.
        assert!(find_ps_entry(&ps, "other:latest").is_none());
        assert_eq!(find_ps_entry(&ps, "bare:latest").unwrap()["size"], 2);
        assert!(find_ps_entry(&json!({}), "bare:latest").is_none());
    }

    #[test]
    fn url_joins_without_doubling_the_slash() {
        assert_eq!(url("http://h:1/", "/api/ps"), "http://h:1/api/ps");
        assert_eq!(url("http://h:1", "/api/ps"), "http://h:1/api/ps");
    }
}
