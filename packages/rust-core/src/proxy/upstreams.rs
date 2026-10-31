//! Other OpenAI-compatible engines behind the same endpoint: LM Studio, a llama.cpp server,
//! vLLM, MLX servers or a cloud API, each named in `upstreams.toml`.
//!
//! Only the OpenAI-compatible paths (`/v1/...`) route by model. `name/model` goes to upstream
//! `name` as `model`; a bare name goes to Ollama when Ollama has it, else to the first upstream
//! whose `/v1/models` lists it. `GET /v1/models` lists Ollama's models and every upstream's,
//! prefixed with the upstream's name. A model list is cached until a request names a model it
//! lacks, which reads the lists again. `FreeLlama`'s own bearer token never leaves: requests to
//! an upstream carry that upstream's key, or none.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};

/// One OpenAI-compatible engine.
#[derive(Clone)]
pub struct OpenAiUpstream {
    pub name: String,
    /// The API root that `/v1/...` paths continue, such as `http://127.0.0.1:1234/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for OpenAiUpstream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiUpstream")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamsFile {
    #[serde(default)]
    upstream: Vec<UpstreamEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamEntry {
    name: String,
    url: String,
    /// Owner-only file holding the API key; relative paths are read from this file's folder.
    api_key_file: Option<std::path::PathBuf>,
}

/// Read an upstreams file (TOML `[[upstream]]` tables: `name`, `url`, optional `api_key_file`).
///
/// # Errors
///
/// Returns an error when the file or a key file cannot be read, or an entry is invalid.
pub fn load_upstreams(path: &Path) -> Result<Vec<OpenAiUpstream>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read upstreams file {}", path.display()))?;
    let file: UpstreamsFile = toml::from_str(&text)
        .with_context(|| format!("parse upstreams file {}", path.display()))?;
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let upstreams = file
        .upstream
        .into_iter()
        .map(|entry| {
            let api_key = entry
                .api_key_file
                .map(|key| {
                    let key = if key.is_absolute() {
                        key
                    } else {
                        folder.join(key)
                    };
                    crate::auth::read_secret_file(&key)
                        .with_context(|| format!("upstream {}", entry.name))
                })
                .transpose()?;
            Ok(OpenAiUpstream {
                name: entry.name,
                base_url: entry.url.trim_end_matches('/').to_owned(),
                api_key,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate(&upstreams)?;
    Ok(upstreams)
}

/// Names are distinct path-safe words; URLs are http(s); a key travels only over https or to
/// this machine.
pub(crate) fn validate(upstreams: &[OpenAiUpstream]) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for upstream in upstreams {
        ensure!(
            !upstream.name.is_empty()
                && upstream
                    .name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
            "upstream name {:?} must be letters, digits, '-', '_' or '.'",
            upstream.name
        );
        ensure!(
            names.insert(upstream.name.as_str()),
            "upstream {} is named twice",
            upstream.name
        );
        let url = Url::parse(&upstream.base_url)
            .with_context(|| format!("upstream {} url", upstream.name))?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "upstream {} url must be http or https",
            upstream.name
        );
        ensure!(
            upstream.api_key.is_none()
                || url.scheme() == "https"
                || url.host_str().is_some_and(crate::net::is_loopback_host),
            "upstream {} sends an API key, so its url must be https or on this machine",
            upstream.name
        );
    }
    Ok(())
}

/// Where one OpenAI-compatible request goes.
pub(crate) enum Route {
    Ollama,
    /// Upstream index, and the model name it knows.
    Upstream(usize, String),
}

#[derive(Clone)]
pub(crate) struct Upstreams {
    pub(crate) list: Arc<[OpenAiUpstream]>,
    /// Model ids each upstream listed when last read.
    models: Arc<Mutex<Vec<Vec<String>>>>,
    /// Ollama's tags when last read.
    ollama: Arc<Mutex<Vec<String>>>,
}

impl Upstreams {
    pub(crate) fn new(list: Vec<OpenAiUpstream>) -> Self {
        let count = list.len();
        Self {
            list: Arc::from(list),
            models: Arc::new(Mutex::new(vec![Vec::new(); count])),
            ollama: Arc::default(),
        }
    }

    pub(crate) fn url(&self, index: usize, path_and_query: &str) -> String {
        let rest = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
        format!("{}{rest}", self.list[index].base_url)
    }

    /// Route a request for `model`.
    pub(crate) async fn route(&self, client: &Client, ollama: &str, model: &str) -> Route {
        if let Some((name, rest)) = model.split_once('/')
            && let Some(index) = self.list.iter().position(|upstream| upstream.name == name)
        {
            return Route::Upstream(index, rest.to_owned());
        }
        for refresh in [false, true] {
            if refresh {
                self.refresh(client, ollama).await;
            }
            let ollama_has = {
                let names = self
                    .ollama
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                crate::ollama_http::resolve_as_ollama(&names, model).is_some()
            };
            if ollama_has {
                return Route::Ollama;
            }
            let listed = self
                .models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .position(|models| models.iter().any(|id| id == model));
            if let Some(index) = listed {
                return Route::Upstream(index, model.to_owned());
            }
        }
        // Nobody lists it: Ollama answers with its own error, as without upstreams.
        Route::Ollama
    }

    async fn refresh(&self, client: &Client, ollama: &str) {
        let tags = get(
            client,
            &format!("{}/api/tags", ollama.trim_end_matches('/')),
            None,
        )
        .await;
        if let Some(tags) = tags {
            *self
                .ollama
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                crate::ollama_http::listed_names(&tags)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
        }
        let lists = self.lists(client).await;
        let mut models = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (index, list) in lists.into_iter().enumerate() {
            if let Some(list) = list {
                models[index] = ids(&list);
            }
        }
    }

    /// Every upstream's `/models`, read at once; `None` for one that did not answer.
    async fn lists(&self, client: &Client) -> Vec<Option<Value>> {
        let mut reads = tokio::task::JoinSet::new();
        for (index, upstream) in self.list.iter().enumerate() {
            let (client, url, key) = (
                client.clone(),
                format!("{}/models", upstream.base_url),
                upstream.api_key.clone(),
            );
            reads.spawn(async move { (index, get(&client, &url, key.as_deref()).await) });
        }
        let mut lists = vec![None; self.list.len()];
        while let Some(Ok((index, list))) = reads.join_next().await {
            lists[index] = list;
        }
        lists
    }

    /// Ollama's OpenAI-compatible model list with every reachable upstream's, prefixed by name.
    /// An unreachable upstream is reported, not hidden.
    pub(crate) async fn merged_models(&self, client: &Client, ollama: &str) -> Value {
        let own = get(
            client,
            &format!("{}/v1/models", ollama.trim_end_matches('/')),
            None,
        )
        .await;
        let mut data = own
            .as_ref()
            .and_then(|own| own["data"].as_array().cloned())
            .unwrap_or_default();
        let lists = self.lists(client).await;
        let mut unreachable = Vec::new();
        let mut models = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (index, list) in lists.into_iter().enumerate() {
            let upstream = &self.list[index];
            let Some(list) = list else {
                unreachable.push(upstream.name.clone());
                continue;
            };
            models[index] = ids(&list);
            for entry in list["data"].as_array().into_iter().flatten() {
                let mut entry = entry.clone();
                if let Some(id) = entry["id"].as_str() {
                    entry["id"] = json!(format!("{}/{id}", upstream.name));
                    entry["owned_by"] = json!(upstream.name);
                }
                data.push(entry);
            }
        }
        drop(models);
        let mut merged = json!({"object": "list", "data": data});
        if own.is_none() {
            unreachable.insert(0, "ollama".into());
        }
        if !unreachable.is_empty() {
            merged["unreachable"] = json!(unreachable);
        }
        merged
    }
}

fn ids(list: &Value) -> Vec<String> {
    list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect()
}

async fn get(client: &Client, url: &str, key: Option<&str>) -> Option<Value> {
    let mut request = client.get(url).timeout(crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    ));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(name: &str, url: &str, key: bool) -> OpenAiUpstream {
        OpenAiUpstream {
            name: name.into(),
            base_url: url.into(),
            api_key: key.then(|| "k".into()),
        }
    }

    #[test]
    fn upstreams_need_distinct_safe_names_and_keys_stay_on_https_or_this_machine() {
        assert!(validate(&[upstream("lmstudio", "http://127.0.0.1:1234/v1", true)]).is_ok());
        assert!(validate(&[upstream("cloud", "https://api.example.com/v1", true)]).is_ok());
        assert!(validate(&[upstream("lan", "http://10.0.0.5:8000/v1", true)]).is_err());
        assert!(validate(&[upstream("lan", "http://10.0.0.5:8000/v1", false)]).is_ok());
        assert!(validate(&[upstream("a/b", "http://127.0.0.1:1/v1", false)]).is_err());
        assert!(
            validate(&[
                upstream("x", "http://127.0.0.1:1/v1", false),
                upstream("x", "http://127.0.0.1:2/v1", false)
            ])
            .is_err()
        );
        assert!(validate(&[upstream("x", "ftp://host/v1", false)]).is_err());
    }

    #[test]
    fn paths_continue_the_upstream_api_root() {
        let upstreams = Upstreams::new(vec![upstream("lm", "http://127.0.0.1:1234/v1", false)]);
        assert_eq!(
            upstreams.url(0, "/v1/chat/completions?x=1"),
            "http://127.0.0.1:1234/v1/chat/completions?x=1"
        );
    }
}
