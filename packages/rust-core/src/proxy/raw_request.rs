//! Raw request classification, the model a request names, and its forecast resource permit.

use crate::platform::resources::{ResourceDemand, ResourcePermit, ResourceWaitError};
use crate::platform::{Backend, PlacementRule};

use super::{ProxyState, RAW_PRESSURE_WAIT, RawForecast};

/// Ollama endpoints that only move model files. They never load a runner, so they neither take
/// the execution lock nor wait on memory, so a multi-minute pull does not refuse managed tasks.
const MODEL_STORE_PATHS: &[&str] = &["/api/pull", "/api/push", "/api/copy", "/api/delete"];

/// How a raw request is admitted. Only generations take the execution lock and memory checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RawRequestKind {
    /// Reads (GET/HEAD, `/api/show`): always available, including to diagnose pressure.
    Metadata,
    /// Pull/push/copy/delete and blob uploads: file work that never loads a runner.
    ModelStore,
    /// A `keep_alive: 0` request with nothing to generate: frees memory, so never held for it.
    Unload,
    /// Anything that may load a model or generate.
    Generation,
}

fn is_zero_keep_alive(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(number) => number.as_f64() == Some(0.0),
        serde_json::Value::String(text) => {
            let digits = text.trim().trim_end_matches(['s', 'm', 'h']);
            !digits.is_empty() && digits.parse::<f64>().is_ok_and(|value| value == 0.0)
        }
        _ => false,
    }
}

pub(super) fn classify(method: &reqwest::Method, path: &str, body: &[u8]) -> RawRequestKind {
    if path.starts_with("/api/blobs/") || MODEL_STORE_PATHS.contains(&path) {
        return RawRequestKind::ModelStore;
    }
    if matches!(*method, reqwest::Method::GET | reqwest::Method::HEAD) || path == "/api/show" {
        return RawRequestKind::Metadata;
    }
    if matches!(path, "/api/generate" | "/api/chat" | "/api/embed")
        && let Ok(body) = serde_json::from_slice::<serde_json::Value>(body)
        && body.get("keep_alive").is_some_and(is_zero_keep_alive)
        && body
            .get("prompt")
            .is_none_or(|value| value.as_str() == Some(""))
        && body
            .get("messages")
            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
        && body.get("input").is_none_or(|value| {
            value.as_str() == Some("") || value.as_array().is_some_and(Vec::is_empty)
        })
        && body.get("images").is_none()
    {
        return RawRequestKind::Unload;
    }
    RawRequestKind::Generation
}

/// Forecast cold allocations even when host telemetry is healthy. Resident reuse needs the
/// compatibility observation already made under the transition guard.
pub(super) async fn raw_resource_permit(
    state: &ProxyState,
    mut model: Option<&mut RawModel>,
    resident: Option<bool>,
) -> Result<ResourcePermit, ResourceWaitError> {
    let governor = &state.resource_governor;
    let cpu_only = model.as_deref().is_some_and(|model| model.cpu_only);
    if model.as_deref().is_some_and(|model| model.not_installed) {
        // Nothing can load, so this claims no memory; Ollama answers with its own "not found".
        return governor
            .wait_for_demand(
                &state.upstream,
                ResourceDemand::resident().on_cpu(cpu_only),
                RAW_PRESSURE_WAIT,
            )
            .await;
    }
    if !crate::net::is_loopback_url(&state.upstream) {
        // Local host telemetry cannot reserve memory on a remote Ollama process.
        return governor
            .wait_for_demand(&state.upstream, ResourceDemand::load(0), RAW_PRESSURE_WAIT)
            .await;
    }
    let resident = match (resident, model.as_deref_mut()) {
        (Some(resident), _) => resident,
        (None, Some(model)) => model.clone().observe(state).await,
        (None, None) => false,
    };
    // A cold load declares its installed size, so a small model (a speech or embedding model)
    // keeps the small-load exemption under swap instead of waiting as a load of unknown size.
    // No resolved model reaches here only for well-formed JSON that names none (the caller
    // refuses an unresolvable body first); Ollama rejects it before scheduling, so it can
    // allocate nothing and claims no memory.
    let demand = match model {
        Some(model) if !resident => {
            ResourceDemand::load(model.forecast_bytes.filter(|bytes| *bytes > 0).ok_or_else(
                || ResourceWaitError {
                    receipt: Box::new(crate::platform::resources::ResourceReceipt {
                        status: "model_forecast_unavailable",
                        waited_ms: 0,
                        required_available_bytes: 0,
                        reserved_bytes: 0,
                        snapshot: None,
                        assessment: None,
                    }),
                },
            )?)
        }
        _ => ResourceDemand::resident(),
    };
    governor
        .wait_for_demand(&state.upstream, demand.on_cpu(cpu_only), RAW_PRESSURE_WAIT)
        .await
}

/// Standalone fallback: installed identity plus file/overhead estimate, without `/api/show`.
async fn installed_forecast(state: &ProxyState, model: &str) -> Option<RawForecast> {
    let response = state
        .client
        .get(format!("{}/api/tags", state.upstream.trim_end_matches('/')))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let tags = response.json::<serde_json::Value>().await.ok()?;
    let entry = crate::ollama_http::find_ps_entry(&tags, model)?;
    let bytes = entry["size"].as_u64().filter(|bytes| *bytes > 0)?;
    Some(RawForecast {
        required_host_bytes: bytes.saturating_add(bytes / 3),
        context_tokens: None,
        digest: entry["digest"]
            .as_str()
            .filter(|digest| !digest.is_empty())
            .map(str::to_owned),
    })
}

/// Whether Ollama's catalog was readable and does not list `model`, as Ollama resolves the name.
async fn not_listed(state: &ProxyState, model: &str) -> bool {
    let tags = async {
        let response = state
            .client
            .get(format!("{}/api/tags", state.upstream.trim_end_matches('/')))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        response.json::<serde_json::Value>().await.ok()
    }
    .await;
    tags.is_some_and(|tags| {
        crate::ollama_http::resolve_as_ollama(&crate::ollama_http::listed_names(&tags), model)
            .is_none()
    })
}

/// The model a raw request names, and the context size it asks for.
#[derive(Clone)]
pub(super) struct RawModel {
    pub(super) name: String,
    pub(super) context: Option<u64>,
    pub(super) options_unverified: bool,
    /// Explicit CPU-only request intent; unknown placement retains conservative GPU gating.
    pub(super) cpu_only: bool,
    pub(super) digest: Option<String>,
    pub(super) forecast_bytes: Option<u64>,
    /// Ollama's catalog was read and does not list this model: nothing can load, and Ollama's own
    /// 404 is the answer. Unknown metadata is different, and stays a refusal.
    pub(super) not_installed: bool,
}

impl RawModel {
    pub(super) fn parse(path: &str, body: &[u8]) -> Option<Self> {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Self::from_multipart(body);
        };
        let mut name = body.get("model")?.as_str()?.to_owned();
        if name.trim().is_empty() {
            return None;
        }
        if !name.rsplit('/').next().unwrap_or(&name).contains(':') {
            name.push_str(":latest");
        }
        // Compatibility adapters discard native runner options; an ignored field cannot prove
        // CPU intent or the context that the upstream runner will use.
        let native_options = matches!(
            path,
            "/api/generate" | "/api/chat" | "/api/embed" | "/api/embeddings"
        );
        Some(Self {
            name,
            context: native_options
                .then(|| {
                    body.pointer("/options/num_ctx")
                        .and_then(serde_json::Value::as_u64)
                })
                .flatten(),
            options_unverified: [
                "num_batch",
                "num_thread",
                "num_gpu",
                "main_gpu",
                "numa",
                "low_vram",
                "f16_kv",
                "logits_all",
                "vocab_only",
                "use_mmap",
                "use_mlock",
                "embedding_only",
            ]
            .iter()
            .any(|key| body["options"].get(*key).is_some()),
            cpu_only: native_options
                && body
                    .pointer("/options/num_gpu")
                    .and_then(serde_json::Value::as_i64)
                    == Some(0),
            digest: None,
            forecast_bytes: None,
            not_installed: false,
        })
    }

    /// The `model` field of a `multipart/form-data` body (`/v1/audio/transcriptions`), whose
    /// value follows the part's blank line and ends at the next CRLF.
    fn from_multipart(body: &[u8]) -> Option<Self> {
        const FIELD: &[u8] = b"name=\"model\"";
        let at = body
            .windows(FIELD.len())
            .position(|window| window == FIELD)?
            + FIELD.len();
        let rest = &body[at..];
        let start = rest.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
        let value = &rest[start..];
        let end = value.windows(2).position(|window| window == b"\r\n")?;
        let name = std::str::from_utf8(&value[..end]).ok()?.trim();
        (!name.is_empty() && name.len() <= 256).then(|| Self {
            name: if name.rsplit('/').next().unwrap_or(name).contains(':') {
                name.to_owned()
            } else {
                format!("{name}:latest")
            },
            context: None,
            options_unverified: true,
            cpu_only: false,
            digest: None,
            forecast_bytes: None,
            not_installed: false,
        })
    }

    /// Whether Ollama runs this model at a compatible context: an explicit `options.num_ctx`
    /// other than the resident `context_length` makes Ollama reload the runner. A bare name
    /// (`gemma3`) means `gemma3:latest` to Ollama, so residency, busy marks and the audit all key
    /// on that tag.
    pub(super) async fn observe(&mut self, state: &ProxyState) -> bool {
        if self.options_unverified {
            return false;
        }
        let get = |path: &'static str| {
            state
                .client
                .get(format!("{}{path}", state.upstream.trim_end_matches('/')))
                .send()
        };
        let Ok(ps) = async { get("/api/ps").await?.json::<serde_json::Value>().await }.await else {
            return false;
        };
        // Resolve exactly as Ollama will read the name it receives: the tag, else `:latest`.
        if let Some(name) = crate::ollama_http::resolve_as_ollama(
            &crate::ollama_http::listed_names(&ps),
            &self.name,
        ) {
            name.clone_into(&mut self.name);
        } else if !self
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&self.name)
            .contains(':')
        {
            self.name.push_str(":latest");
        }
        self.transition_compatible(&ps)
    }

    pub(super) async fn hydrate(&mut self, state: &ProxyState) {
        let forecast = if let Some(hooks) = &state.hooks {
            hooks.forecast(&self.name, self.context).await
        } else {
            None
        };
        let forecast = match forecast {
            Some(forecast) => Some(forecast),
            None => installed_forecast(state, &self.name).await,
        };
        if let Some(forecast) = forecast {
            self.context = self.context.or(forecast.context_tokens);
            self.digest = forecast.digest;
            self.forecast_bytes = Some(forecast.required_host_bytes);
        } else {
            self.not_installed = not_listed(state, &self.name).await;
        }
        // No num_ctx in the request and no derivable default: use the window Ollama gave this
        // model for such a request before.
        if self.context.is_none() {
            self.context = state
                .learned_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(super::context_key(&self.name))
                .copied();
        }
    }

    /// Both initial sharing and an early transition release require known identity and exact
    /// effective context. Unreported runtime controls and unknown defaults remain conservative.
    pub(super) fn transition_compatible(&self, ps: &serde_json::Value) -> bool {
        !self.options_unverified
            && crate::ollama_http::ps_entries(ps, &self.name).any(|entry| {
                self.context
                    .filter(|context| *context > 0)
                    .is_some_and(|context| entry["context_length"].as_u64() == Some(context))
                    && self.digest.as_deref().is_some_and(|digest| {
                        !digest.is_empty() && entry["digest"].as_str() == Some(digest)
                    })
                    // Raw callers state no placement evidence requirement, so the shared
                    // rule imposes none here; it is the managed rule's `Any`.
                    && PlacementRule::for_backend(Backend::Gpu, false, false).satisfied_by(entry)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{RawModel, RawRequestKind, classify};
    use reqwest::Method;

    #[test]
    fn raw_initial_reuse_and_transition_release_require_exact_known_identity_and_context() {
        let mut model =
            RawModel::parse("/api/chat", br#"{"model":"m","options":{"num_ctx":4096}}"#).unwrap();
        let exact =
            serde_json::json!({"models":[{"name":"m:latest","digest":"a","context_length":4096}]});
        assert!(!model.transition_compatible(&exact));
        model.digest = Some("a".into());
        assert!(model.transition_compatible(&exact));
        model.digest = Some("replacement".into());
        assert!(
            !model.transition_compatible(&exact),
            "a replaced installed tag cannot reuse its old resident runner"
        );
        model.digest = Some("a".into());
        for (digest, context) in [("b", 4096), ("a", 2048), ("a", 8192), ("a", 0)] {
            assert!(!model.transition_compatible(&serde_json::json!({"models":[{"name":"m:latest","digest":digest,"context_length":context}]})));
        }
        model.context = None;
        assert!(!model.transition_compatible(&exact));
        model.context = Some(4096);
        model.options_unverified = true;
        assert!(!model.transition_compatible(&exact));
    }

    #[test]
    fn raw_cpu_only_intent_requires_an_explicit_integer_zero() {
        for (value, cpu_only) in [
            (serde_json::json!(0), true),
            (serde_json::json!(2), false),
            (serde_json::json!(-1), false),
            (serde_json::json!("0"), false),
            (serde_json::Value::Null, false),
        ] {
            let body = serde_json::json!({"model":"m","options":{"num_gpu":value}});
            let model = RawModel::parse("/api/chat", body.to_string().as_bytes()).unwrap();
            assert_eq!(model.cpu_only, cpu_only);
            assert!(
                model.options_unverified,
                "request intent does not establish runner compatibility"
            );
        }
        assert!(
            !RawModel::parse("/api/chat", br#"{"model":"m"}"#)
                .unwrap()
                .cpu_only
        );
    }

    #[test]
    fn raw_runner_options_require_a_native_ollama_endpoint() {
        let body = br#"{"model":"m","options":{"num_gpu":0,"num_ctx":4096}}"#;
        for path in [
            "/api/generate",
            "/api/chat",
            "/api/embed",
            "/api/embeddings",
        ] {
            let model = RawModel::parse(path, body).unwrap();
            assert!(model.cpu_only, "{path}");
            assert_eq!(model.context, Some(4096), "{path}");
            assert!(model.options_unverified);
        }
        for path in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/responses",
            "/v1/messages",
            "/v1/audio/speech",
            "/api/unknown",
        ] {
            let model = RawModel::parse(path, body).unwrap();
            assert!(!model.cpu_only, "{path}");
            assert_eq!(model.context, None, "{path}");
            assert!(model.options_unverified);
        }
    }

    #[test]
    fn a_multipart_audio_request_names_its_model() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\nRIFF\r\n--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nlfm3-audio:2b\r\n--b--\r\n";
        let model = RawModel::parse("/v1/audio/transcriptions", body).expect("model field");
        assert_eq!(model.name, "lfm3-audio:2b");
        assert!(
            RawModel::parse(
                "/v1/audio/transcriptions",
                b"--b\r\nname=\"file\"\r\n\r\nRIFF\r\n--b--"
            )
            .is_none()
        );
        assert_eq!(
            RawModel::parse("/v1/audio/speech", br#"{"model":"phonon:en","input":"hi"}"#)
                .map(|model| model.name),
            Some("phonon:en".to_owned())
        );
    }

    #[test]
    fn an_empty_request_with_zero_keep_alive_is_an_unload_on_every_load_path() {
        for (path, body) in [
            ("/api/generate", r#"{"model":"m","keep_alive":0}"#),
            (
                "/api/chat",
                r#"{"model":"m","messages":[],"keep_alive":"0"}"#,
            ),
            ("/api/embed", r#"{"model":"m","input":[],"keep_alive":0}"#),
        ] {
            assert_eq!(
                classify(&Method::POST, path, body.as_bytes()),
                RawRequestKind::Unload,
                "{path}"
            );
        }
        for (path, body) in [
            (
                "/api/embed",
                r#"{"model":"m","input":["hi"],"keep_alive":0}"#,
            ),
            (
                "/api/embed",
                r#"{"model":"m","input":[],"keep_alive":"5m"}"#,
            ),
            (
                "/api/generate",
                r#"{"model":"m","prompt":"hi","keep_alive":0}"#,
            ),
        ] {
            assert_eq!(
                classify(&Method::POST, path, body.as_bytes()),
                RawRequestKind::Generation,
                "{body}"
            );
        }
    }
}
