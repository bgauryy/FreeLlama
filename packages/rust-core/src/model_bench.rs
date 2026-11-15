//! Capability-aware benchmarks for every model exposed by an Ollama server.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;

use crate::capability::{Capability, ModelType};
use crate::ollama_http::{millis, resident, unload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod provenance;
pub use provenance::BenchmarkMachine;

const RED_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40, 0x08, 0x02, 0x00, 0x00, 0x00, 0x25, 0x0b, 0xe6,
    0x89, 0x00, 0x00, 0x00, 0x09, 0x70, 0x48, 0x59, 0x73, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x4f, 0x25, 0xc4, 0xd6, 0x00, 0x00, 0x00, 0x70, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c,
    0xed, 0xcf, 0x41, 0x09, 0x00, 0x00, 0x10, 0x84, 0xc0, 0xed, 0x9f, 0xf9, 0x1e, 0x17, 0x43, 0x04,
    0x61, 0x02, 0xe8, 0x6e, 0x53, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a,
    0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a,
    0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a,
    0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a,
    0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a,
    0x90, 0xe3, 0x0b, 0x1a, 0x90, 0xe3, 0x0b, 0x1a, 0x90, 0x7b, 0x36, 0x02, 0xc0, 0xe2, 0xc3, 0x57,
    0x81, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub name: String,
    /// Artifact identity recorded at discovery; a report without it cannot qualify routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub size: u64,
    pub format: String,
    pub family: String,
    pub parameter_size: String,
    pub quantization: String,
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub model_type: ModelType,
    pub advertised_context: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BenchCase {
    pub id: &'static str,
    pub capability: Capability,
    pub num_ctx: u32,
    kind: CaseKind,
}

#[derive(Debug, Clone, Copy)]
enum CaseKind {
    Exact,
    Math,
    Needle,
    Tool,
    ToolRecovery,
    Vision,
    Embedding,
    Decision,
}

/// Build a fixed, capability-specific case plan using conservative model-size tiers.
///
/// These tiers make benchmark cases comparable across machines. They are not a hardware profile
/// or a claim that a model fits; task routing and admission perform that separate check
/// against the discovered host-memory budget and the context actually sent.
#[must_use]
pub fn benchmark_plan(model: &ModelMetadata) -> Vec<BenchCase> {
    let context = if model.size >= 16_000_000_000 {
        8_192
    } else if model.size >= 2_000_000_000 {
        16_384
    } else {
        32_768
    };
    let supported_context = model
        .advertised_context
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(context);
    let num_ctx = context.min(supported_context);
    let has = |capability| model.capabilities.contains(&capability);
    // Tools and vision are exercised through `/api/chat`, which only a completion model serves:
    // an embedding model can advertise `vision` (its encoder reads images) yet refuses a chat.
    let chats = has(Capability::Completion);
    let mut cases = Vec::new();
    if chats {
        cases.push(BenchCase {
            id: "text/exact",
            capability: Capability::Completion,
            num_ctx,
            kind: CaseKind::Exact,
        });
        cases.push(BenchCase {
            id: "text/math",
            capability: Capability::Completion,
            num_ctx,
            kind: CaseKind::Math,
        });
        cases.push(BenchCase {
            id: "text/long-needle",
            capability: Capability::Completion,
            num_ctx,
            kind: CaseKind::Needle,
        });
    }
    if chats && has(Capability::Tools) {
        cases.push(BenchCase {
            id: "tools/multiply",
            capability: Capability::Tools,
            num_ctx,
            kind: CaseKind::Tool,
        });
        cases.push(BenchCase {
            id: "tools/recovery",
            capability: Capability::Tools,
            num_ctx,
            kind: CaseKind::ToolRecovery,
        });
    }
    if chats && has(Capability::Vision) {
        cases.push(BenchCase {
            id: "vision/color",
            capability: Capability::Vision,
            num_ctx,
            kind: CaseKind::Vision,
        });
    }
    if has(Capability::Embedding) {
        cases.push(BenchCase {
            id: "embedding/integrity",
            capability: Capability::Embedding,
            num_ctx,
            kind: CaseKind::Embedding,
        });
    }
    if has(Capability::Decision) {
        cases.push(BenchCase {
            id: "decision/noul",
            capability: Capability::Decision,
            num_ctx,
            kind: CaseKind::Decision,
        });
    }
    cases
}

/// Quality-guarded useful cases completed per hour.
#[must_use]
pub fn score_cases(passed: usize, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    f64_count(passed) * 3_600.0 / Duration::from_millis(elapsed_ms).as_secs_f64()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchConfig {
    pub endpoint: String,
    pub include: Vec<String>,
    pub timeout_seconds: u64,
    pub trials: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllModelsReport {
    pub schema_version: u32,
    /// Absent on old reports and remote endpoints: they cannot qualify local throughput.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<BenchmarkMachine>,
    #[serde(default)]
    pub measured_at_unix_seconds: u64,
    pub endpoint: String,
    pub server_version: String,
    pub profile: String,
    #[serde(default)]
    pub configuration: BenchmarkConfiguration,
    pub methodology: Vec<String>,
    pub models: Vec<ModelBenchResult>,
    pub rankings: BTreeMap<Capability, Vec<RankingEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfiguration {
    pub warm_trials_per_case: u32,
    pub temperature: f64,
    pub seed: u64,
    pub think: bool,
    pub num_predict: u32,
    pub keep_alive: String,
    pub cache_token_metrics: String,
}

impl Default for BenchmarkConfiguration {
    fn default() -> Self {
        Self {
            warm_trials_per_case: 0,
            temperature: 0.0,
            seed: 42,
            think: false,
            num_predict: 128,
            keep_alive: "5m".to_owned(),
            cache_token_metrics: "not_reported_by_ollama".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBenchResult {
    pub metadata: ModelMetadata,
    pub cold_start: Option<ModelCaseResult>,
    pub cases: Vec<ModelCaseResult>,
    pub resident_size: Option<u64>,
    pub resident_vram: Option<u64>,
    /// Failed unload requests; a failure before the cold start means that timing may be warm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unload_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCaseResult {
    pub id: String,
    pub capability: Capability,
    pub passed: bool,
    pub client_ms: u64,
    pub load_ms: Option<f64>,
    pub prompt_tokens_per_second: Option<f64>,
    pub decode_tokens_per_second: Option<f64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    pub num_ctx: u32,
    pub output_excerpt: Option<String>,
    pub error: Option<String>,
    /// How a graded trial's digest was confirmed when the usual residency evidence was missing,
    /// e.g. another client evicted the model after the response completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingEntry {
    pub model: String,
    pub passed: usize,
    pub attempted: usize,
    pub quality_rate: f64,
    pub useful_cases_per_hour: f64,
    pub mean_decode_tokens_per_second: Option<f64>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub contexts_tested: Vec<u32>,
}

/// Discover and sequentially benchmark all installed models.
///
/// # Errors
///
/// Returns an error when Ollama cannot be reached or its discovery responses are invalid.
pub async fn benchmark_all(config: &BenchConfig) -> Result<AllModelsReport> {
    let machine = if crate::net::is_loopback_url(&config.endpoint) {
        tokio::task::spawn_blocking(BenchmarkMachine::current).await?
    } else {
        None
    };
    let client = crate::net::build_http_client(|builder| {
        builder.timeout(Duration::from_secs(config.timeout_seconds))
    })
    .context("build benchmark HTTP client")?;
    let server_version = get_json(
        &client,
        &crate::ollama_http::url(&config.endpoint, "/api/version"),
    )
    .await?
    .get("version")
    .and_then(Value::as_str)
    .unwrap_or("unknown")
    .to_owned();
    let models = discover_models(&client, &config.endpoint).await?;
    ensure!(config.trials > 0, "benchmark requires at least one trial");
    for name in &config.include {
        ensure!(
            models.iter().any(|model| &model.name == name),
            "benchmark requires an exact installed tag: {name}"
        );
    }
    let selected = models
        .into_iter()
        .filter(|model| {
            config.include.is_empty() || config.include.iter().any(|name| name == &model.name)
        })
        .collect::<Vec<_>>();
    ensure!(
        !selected.is_empty(),
        "no installed models matched the selection"
    );

    let mut results = Vec::with_capacity(selected.len());
    for model in selected {
        results.push(benchmark_model(&client, config, model).await);
    }
    let rankings = build_rankings(&results);
    Ok(AllModelsReport {
        schema_version: 2,
        machine,
        measured_at_unix_seconds: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
        endpoint: config.endpoint.clone(),
        server_version,
        profile: "sequential_local_balanced_v2".to_owned(),
        configuration: BenchmarkConfiguration {
            warm_trials_per_case: config.trials,
            ..BenchmarkConfiguration::default()
        },
        methodology: vec![
            "Models run sequentially to avoid unified-memory contention.".to_owned(),
            format!("A first request is reported separately only when /api/ps initially shows the model absent; each case then runs {} trials. Existing resident models are never unloaded by this benchmark.", config.trials),
            "Large artifacts use an 8K evaluation context; medium 16K; small 32K, capped by the advertised context.".to_owned(),
            "The deterministic screen uses temperature 0, seed 42, thinking disabled, and a 128-token response-envelope budget.".to_owned(),
            "Decision models answer one fixed state on /v1/systemone (no options) with a clearly true and a clearly false noul question; both must be answered confidently and correctly.".to_owned(),
            "Each trial's digest is checked in /api/tags before dispatch and in /api/ps after completion, falling back to /api/tags when another client evicted the model.".to_owned(),
            "Capability groups are ranked independently by quality-guarded useful cases/hour.".to_owned(),
            "This is a functional throughput screen, not a broad knowledge or reasoning leaderboard.".to_owned(),
        ],
        models: results,
        rankings,
    })
}

async fn benchmark_model(
    client: &Client,
    config: &BenchConfig,
    model: ModelMetadata,
) -> ModelBenchResult {
    eprintln!(
        "benchmark model={} capabilities={:?}",
        model.name, model.capabilities
    );
    let plan = benchmark_plan(&model);
    // Only clean up a model this run found absent before it warmed it. Never evict an
    // operator's already-loaded runner to manufacture a cold timing.
    let initially_absent = get_json(
        client,
        &crate::ollama_http::url(&config.endpoint, "/api/ps"),
    )
    .await
    .ok()
    .and_then(|value| {
        value.get("models").and_then(Value::as_array).map(|models| {
            !models
                .iter()
                .any(|entry| crate::ollama_http::ps_entry_name(entry) == Some(model.name.as_str()))
        })
    })
    .unwrap_or(false);
    let mut unload_errors = Vec::new();
    let cold_start = if initially_absent && let Some(case) = plan.first() {
        Some(run_case(client, &config.endpoint, &model, case).await)
    } else {
        None
    };
    let mut cases = Vec::with_capacity(
        plan.len()
            .saturating_mul(usize::try_from(config.trials).unwrap_or(usize::MAX)),
    );
    for trial in 1..=config.trials {
        for case in &plan {
            let mut result = run_case(client, &config.endpoint, &model, case).await;
            result.id = format!("{}/trial-{trial}", result.id);
            cases.push(result);
        }
    }
    let (resident_size, resident_vram) = resident(client, &config.endpoint, &model.name).await;
    let completed = cold_start
        .as_ref()
        .into_iter()
        .chain(&cases)
        .any(|case| case.error.is_none());
    if initially_absent && completed {
        match verify_dispatch_identity(client, &config.endpoint, &model).await {
            Ok(()) => {
                if let Err(error) = unload(client, &config.endpoint, &model.name).await {
                    unload_errors.push(format!("after trials: {error:#}"));
                }
            }
            Err(error) => unload_errors.push(format!("cleanup skipped: {error}")),
        }
    }
    ModelBenchResult {
        metadata: model,
        cold_start,
        cases,
        resident_size,
        resident_vram,
        unload_errors,
    }
}

async fn discover_models(client: &Client, endpoint: &str) -> Result<Vec<ModelMetadata>> {
    let tags = get_json(client, &crate::ollama_http::url(endpoint, "/api/tags")).await?;
    let entries = tags
        .get("models")
        .and_then(Value::as_array)
        .context("tags response has no models")?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .context("model has no name")?;
        let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
        let show = client
            .post(crate::ollama_http::url(endpoint, "/api/show"))
            .json(&json!({"model": name}))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let details = show.get("details").cloned().unwrap_or(Value::Null);
        // Unknown upstream strings stay visible in raw `/api/show`, but must not collapse into one
        // routable `Other` value: two unrelated future capabilities are not interchangeable.
        let capabilities: Vec<Capability> = show
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(Capability::parse)
            .filter(|capability| *capability != Capability::Other)
            .collect();
        let advertised_context =
            show.get("model_info")
                .and_then(Value::as_object)
                .and_then(|info| {
                    info.iter()
                        .find(|(key, _)| key.ends_with(".context_length"))
                        .and_then(|(_, value)| value.as_u64())
                });
        let model_type = ModelType::from_capabilities(capabilities.iter().copied());
        models.push(ModelMetadata {
            name: name.to_owned(),
            digest: entry
                .get("digest")
                .and_then(Value::as_str)
                .filter(|digest| !digest.is_empty())
                .map(str::to_owned),
            size,
            format: string_field(&details, "format"),
            family: string_field(&details, "family"),
            parameter_size: string_field(&details, "parameter_size"),
            quantization: string_field(&details, "quantization_level"),
            capabilities,
            model_type,
            advertised_context,
        });
    }
    Ok(models)
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

async fn run_case(
    client: &Client,
    endpoint: &str,
    model: &ModelMetadata,
    case: &BenchCase,
) -> ModelCaseResult {
    if let Err(error) = verify_dispatch_identity(client, endpoint, model).await {
        return failed(case, 0, error);
    }
    let started = Instant::now();
    let response = match case.kind {
        CaseKind::Embedding => client.post(crate::ollama_http::url(endpoint, "/api/embed"))
            .json(&json!({"model": model.name, "input": ["local model benchmark", "local model benchmark"], "keep_alive": "5m"})).send().await,
        CaseKind::Decision => client.post(crate::ollama_http::url(endpoint, "/v1/systemone"))
            .json(&decision_request(model)).send().await,
        _ => client.post(crate::ollama_http::url(endpoint, "/api/chat"))
            .json(&chat_request(model, case)).send().await,
    };
    let value = match response {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => value,
                Err(error) => {
                    return failed(
                        case,
                        millis(started.elapsed()),
                        format!("decode response: {error}"),
                    );
                }
            },
            Err(error) => return failed(case, millis(started.elapsed()), format!("HTTP: {error}")),
        },
        Err(error) => return failed(case, millis(started.elapsed()), format!("request: {error}")),
    };
    let client_ms = millis(started.elapsed());
    let note = match verify_completion_identity(client, endpoint, model).await {
        Ok(note) => note,
        Err(error) => return failed(case, client_ms, error),
    };
    let mut result = grade_completed_case(case, client_ms, &value);
    result.note = note;
    result
}

/// Grade only a completed operation. Correct-looking partial output or an upstream error must
/// never qualify an artifact; embeddings intentionally do not use the chat completion envelope.
fn grade_completed_case(case: &BenchCase, client_ms: u64, value: &Value) -> ModelCaseResult {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return failed(
            case,
            client_ms,
            "benchmark response contains an upstream error".to_owned(),
        );
    }
    if matches!(case.kind, CaseKind::Decision) {
        return grade_decision(case, client_ms, value);
    }
    if !matches!(case.kind, CaseKind::Embedding) {
        if value["done"] != true {
            return failed(
                case,
                client_ms,
                "chat benchmark response did not complete with done:true".to_owned(),
            );
        }
        let message = &value["message"];
        let payload_present = match case.kind {
            CaseKind::Tool | CaseKind::ToolRecovery => message["tool_calls"].is_array(),
            _ => message["content"].is_string(),
        };
        if message["role"] != "assistant" || !payload_present {
            return failed(
                case,
                client_ms,
                "chat benchmark response lacks the required assistant message shape".to_owned(),
            );
        }
    }
    let output = value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let passed = grade(case.kind, value, output);
    ModelCaseResult {
        id: case.id.to_owned(),
        capability: case.capability,
        passed,
        client_ms,
        load_ms: ns_ms(value.get("load_duration").and_then(Value::as_u64)),
        prompt_tokens_per_second: rate(value, "prompt_eval_count", "prompt_eval_duration"),
        decode_tokens_per_second: rate(value, "eval_count", "eval_duration"),
        prompt_tokens: value.get("prompt_eval_count").and_then(Value::as_u64),
        output_tokens: value.get("eval_count").and_then(Value::as_u64),
        num_ctx: case.num_ctx,
        output_excerpt: (!output.is_empty()).then(|| output.chars().take(160).collect()),
        error: (!passed).then(|| "response did not satisfy the deterministic grader".to_owned()),
        note: None,
    }
}

/// `/v1/systemone` has no chat envelope or `done` flag: a completed answer is an `answers` object
/// holding every asked question as a `noul` probability in [0, 1]. The state makes one question
/// clearly true and the other clearly false, so a model answering every question the same way,
/// or hedging near 0.5, fails.
fn grade_decision(case: &BenchCase, client_ms: u64, value: &Value) -> ModelCaseResult {
    let probability = |name: &str| {
        let answer = value.get("answers")?.get(name)?;
        (answer["type"] == "noul")
            .then(|| answer["noul"].as_f64())
            .flatten()
            .filter(|p| (0.0..=1.0).contains(p))
    };
    let (Some(yes), Some(no)) = (
        probability(DECISION_TRUE_QUESTION),
        probability(DECISION_FALSE_QUESTION),
    ) else {
        return failed(
            case,
            client_ms,
            "decision benchmark response lacks a noul probability for every question".to_owned(),
        );
    };
    let passed = yes >= DECISION_CONFIDENT && no <= 1.0 - DECISION_CONFIDENT;
    let usage = |field: &str| {
        value
            .pointer(&format!("/usage/{field}"))
            .and_then(Value::as_u64)
    };
    ModelCaseResult {
        id: case.id.to_owned(),
        capability: case.capability,
        passed,
        client_ms,
        load_ms: ns_ms(value.get("load_duration").and_then(Value::as_u64)),
        prompt_tokens_per_second: None,
        decode_tokens_per_second: None,
        prompt_tokens: usage("input_tokens"),
        output_tokens: usage("output_tokens"),
        num_ctx: case.num_ctx,
        output_excerpt: Some(format!(
            "{DECISION_TRUE_QUESTION}={yes:.3} {DECISION_FALSE_QUESTION}={no:.3}"
        )),
        error: (!passed).then(|| {
            format!(
                "decision probabilities were not confident and correct (need p(true) >= {DECISION_CONFIDENT} and p(false question) <= {:.1})",
                1.0 - DECISION_CONFIDENT
            )
        }),
        note: None,
    }
}

/// Discovery establishes the expected artifact; every trial must still name that artifact
/// immediately before dispatch and be confirmed as that artifact after completion. A mutable tag
/// alone is not sufficient evidence for a digest-bound benchmark score.
async fn verify_dispatch_identity(
    client: &Client,
    endpoint: &str,
    model: &ModelMetadata,
) -> Result<(), String> {
    let expected = expected_digest(model)?;
    let tags = get_json(client, &crate::ollama_http::url(endpoint, "/api/tags")).await;
    match observe_digest(tags.as_ref().ok(), &model.name, expected) {
        DigestObservation::Matches => Ok(()),
        DigestObservation::Differs => Err(DIGEST_CHANGED.to_owned()),
        DigestObservation::Absent => Err(DIGEST_UNOBSERVED.to_owned()),
        DigestObservation::Unavailable => Err(identity_unavailable(&tags)),
    }
}

/// After a completed response the loaded runner (`/api/ps`) is the strongest evidence of which
/// artifact answered. Another Ollama client may evict the model between the response and this
/// check; then `/api/tags` must still name the same digest, which binds the score to the artifact
/// just as the pre-dispatch check does. Any observed different digest fails the trial.
///
/// Returns a note when the digest was confirmed only through `/api/tags`.
async fn verify_completion_identity(
    client: &Client,
    endpoint: &str,
    model: &ModelMetadata,
) -> Result<Option<String>, String> {
    let expected = expected_digest(model)?;
    let ps = get_json(client, &crate::ollama_http::url(endpoint, "/api/ps")).await;
    let resident = observe_digest(ps.as_ref().ok(), &model.name, expected);
    let listed = if matches!(
        resident,
        DigestObservation::Absent | DigestObservation::Unavailable
    ) {
        let tags = get_json(client, &crate::ollama_http::url(endpoint, "/api/tags")).await;
        observe_digest(tags.as_ref().ok(), &model.name, expected)
    } else {
        DigestObservation::Unavailable
    };
    completion_identity(resident, listed)
}

/// The post-completion verdict from the `/api/ps` observation and, when that could not confirm the
/// model, the `/api/tags` fallback.
fn completion_identity(
    resident: DigestObservation,
    listed: DigestObservation,
) -> Result<Option<String>, String> {
    match (resident, listed) {
        (DigestObservation::Matches, _) => Ok(None),
        (DigestObservation::Differs, _) | (_, DigestObservation::Differs) => {
            Err(DIGEST_CHANGED.to_owned())
        }
        (_, DigestObservation::Matches) => Ok(Some(match resident {
            DigestObservation::Absent => "model was evicted after the response completed; digest confirmed through /api/tags".to_owned(),
            _ => "/api/ps was unavailable after the response completed; digest confirmed through /api/tags".to_owned(),
        })),
        _ => Err(DIGEST_UNOBSERVED.to_owned()),
    }
}

const DIGEST_CHANGED: &str = "benchmark model digest changed; re-run against a stable artifact";
const DIGEST_UNOBSERVED: &str = "benchmark model digest cannot be observed in /api/ps or /api/tags; re-run against a stable artifact";

fn expected_digest(model: &ModelMetadata) -> Result<&str, String> {
    model
        .digest
        .as_deref()
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| {
            "benchmark model digest unavailable; cannot qualify measured evidence".to_owned()
        })
}

fn identity_unavailable(listing: &Result<Value>) -> String {
    match listing {
        Err(error) => format!("benchmark identity unavailable: {error:#}"),
        Ok(_) => DIGEST_UNOBSERVED.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestObservation {
    /// An entry for the model carries the expected digest.
    Matches,
    /// Entries for the model carry digests, none of them the expected one.
    Differs,
    /// The listing was read but holds no digest for the model.
    Absent,
    /// The listing could not be read or is not a model listing.
    Unavailable,
}

/// What an `/api/ps` or `/api/tags` listing says about `name`. Ollama can list one name several
/// times (contexts or digests), so every same-name entry is considered.
fn observe_digest(listing: Option<&Value>, name: &str, expected: &str) -> DigestObservation {
    let Some(listing) = listing.filter(|listing| listing["models"].is_array()) else {
        return DigestObservation::Unavailable;
    };
    let mut digests = crate::ollama_http::ps_entries(listing, name)
        .filter_map(|entry| entry["digest"].as_str())
        .filter(|digest| !digest.is_empty())
        .peekable();
    if digests.peek().is_none() {
        DigestObservation::Absent
    } else if digests.any(|digest| digest == expected) {
        DigestObservation::Matches
    } else {
        DigestObservation::Differs
    }
}

fn chat_request(model: &ModelMetadata, case: &BenchCase) -> Value {
    let num_predict = 128;
    let messages = match case.kind {
        CaseKind::Exact => {
            json!([{"role": "user", "content": "Reply with exactly FREELLAMA_OK and nothing else."}])
        }
        CaseKind::Math => {
            json!([{"role": "user", "content": "Calculate 17 * 23. Reply with only the integer."}])
        }
        CaseKind::Needle => json!([{"role": "user", "content": needle_prompt()}]),
        CaseKind::Tool => {
            json!([{"role": "user", "content": "Use the multiply tool to calculate 17 * 23."}])
        }
        CaseKind::ToolRecovery => json!([
            {"role": "user", "content": "Use divide to calculate 10 / 0."},
            {"role": "assistant", "content": "", "tool_calls": [{"type": "function", "function": {"name": "divide", "arguments": {"a": 10, "b": 0}}}]},
            {"role": "tool", "content": "error: division by zero"},
            {"role": "user", "content": "Recover from the tool error by calling divide again with divisor 2."}
        ]),
        CaseKind::Vision => {
            json!([{"role": "user", "content": "What is the dominant color in this image? Reply with one color word.", "images": [STANDARD.encode(RED_PIXEL_PNG)]}])
        }
        CaseKind::Embedding | CaseKind::Decision => unreachable!(),
    };
    let mut request = json!({
        "model": model.name,
        "messages": messages,
        "stream": false, "think": false, "keep_alive": "5m",
        "options": {"temperature": 0, "seed": 42, "num_predict": num_predict, "num_ctx": case.num_ctx}
    });
    if matches!(case.kind, CaseKind::Tool) {
        request["tools"] = json!([{"type":"function","function":{"name":"multiply","description":"Multiply two integers","parameters":{"type":"object","required":["a","b"],"properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}}}]);
    } else if matches!(case.kind, CaseKind::ToolRecovery) {
        request["tools"] = json!([{"type":"function","function":{"name":"divide","description":"Divide one number by another","parameters":{"type":"object","required":["a","b"],"properties":{"a":{"type":"number"},"b":{"type":"number"}}}}}]);
    }
    request
}

const DECISION_TRUE_QUESTION: &str = "stopped";
const DECISION_FALSE_QUESTION: &str = "moving";
/// The probability a decision model must reach on the clearly true question, and stay below
/// (as `1 - DECISION_CONFIDENT`) on the clearly false one.
const DECISION_CONFIDENT: f64 = 0.7;

/// One `/v1/systemone` request in the shape the platform's decision tasks send: a state and named
/// `noul` questions, with `keep_alive` as the only residency control (the endpoint takes no
/// options, so no seed or temperature is sent; a decision is one forward pass).
fn decision_request(model: &ModelMetadata) -> Value {
    json!({
        "model": model.name,
        "state": "The traffic light is red. Every car at the intersection has come to a complete stop and none is moving.",
        "questions": {
            DECISION_TRUE_QUESTION: {"type": "noul", "instructions": "Are the cars stopped?"},
            DECISION_FALSE_QUESTION: {"type": "noul", "instructions": "Are the cars moving through the intersection?"}
        },
        "keep_alive": "5m",
    })
}

fn grade(kind: CaseKind, value: &Value, output: &str) -> bool {
    match kind {
        CaseKind::Exact => output.trim() == "FREELLAMA_OK",
        CaseKind::Math => output.trim() == "391",
        CaseKind::Needle => output.trim() == "ALPINE-4721",
        CaseKind::Vision => output
            .to_ascii_lowercase()
            .split(|c: char| !c.is_alphabetic())
            .any(|word| word == "red"),
        CaseKind::Tool => {
            value
                .pointer("/message/tool_calls/0/function/name")
                .and_then(Value::as_str)
                == Some("multiply")
                && value
                    .pointer("/message/tool_calls/0/function/arguments/a")
                    .and_then(Value::as_i64)
                    == Some(17)
                && value
                    .pointer("/message/tool_calls/0/function/arguments/b")
                    .and_then(Value::as_i64)
                    == Some(23)
        }
        CaseKind::ToolRecovery => {
            value
                .pointer("/message/tool_calls/0/function/name")
                .and_then(Value::as_str)
                == Some("divide")
                && value
                    .pointer("/message/tool_calls/0/function/arguments/a")
                    .and_then(Value::as_f64)
                    == Some(10.0)
                && value
                    .pointer("/message/tool_calls/0/function/arguments/b")
                    .and_then(Value::as_f64)
                    == Some(2.0)
        }
        CaseKind::Embedding => value
            .get("embeddings")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.len() == 2
                    && items.iter().all(|item| {
                        item.as_array()
                            .is_some_and(|embedding| !embedding.is_empty())
                    })
                    && items[0].as_array().map(Vec::len) == items[1].as_array().map(Vec::len)
            }),
        // Graded by `grade_decision`; a decision answer never satisfies a chat grader.
        CaseKind::Decision => false,
    }
}

fn failed(case: &BenchCase, client_ms: u64, error: String) -> ModelCaseResult {
    ModelCaseResult {
        id: case.id.to_owned(),
        capability: case.capability,
        passed: false,
        client_ms,
        load_ms: None,
        prompt_tokens_per_second: None,
        decode_tokens_per_second: None,
        prompt_tokens: None,
        output_tokens: None,
        num_ctx: case.num_ctx,
        output_excerpt: None,
        error: Some(error),
        note: None,
    }
}

fn build_rankings(results: &[ModelBenchResult]) -> BTreeMap<Capability, Vec<RankingEntry>> {
    let mut rankings = BTreeMap::new();
    for capability in [
        Capability::Completion,
        Capability::Tools,
        Capability::Vision,
        Capability::Embedding,
        Capability::Decision,
    ] {
        let mut entries = results
            .iter()
            .filter_map(|model| {
                let cases = model
                    .cases
                    .iter()
                    .filter(|case| case.capability == capability)
                    .collect::<Vec<_>>();
                if cases.is_empty() {
                    return None;
                }
                let passed = cases.iter().filter(|case| case.passed).count();
                let elapsed = cases.iter().map(|case| case.client_ms).sum();
                let rates = cases
                    .iter()
                    .filter_map(|case| case.decode_tokens_per_second)
                    .collect::<Vec<_>>();
                let prompt_tokens = cases.iter().filter_map(|case| case.prompt_tokens).sum();
                let output_tokens = cases.iter().filter_map(|case| case.output_tokens).sum();
                let contexts_tested = cases
                    .iter()
                    .map(|case| case.num_ctx)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                Some(RankingEntry {
                    model: model.metadata.name.clone(),
                    passed,
                    attempted: cases.len(),
                    quality_rate: f64_count(passed) / f64_count(cases.len()),
                    useful_cases_per_hour: score_cases(passed, elapsed),
                    mean_decode_tokens_per_second: (!rates.is_empty())
                        .then(|| rates.iter().sum::<f64>() / f64_count(rates.len())),
                    prompt_tokens,
                    output_tokens,
                    contexts_tested,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .useful_cases_per_hour
                .total_cmp(&left.useful_cases_per_hour)
        });
        if !entries.is_empty() {
            rankings.insert(capability, entries);
        }
    }
    rankings
}

async fn get_json(client: &Client, url: &str) -> Result<Value> {
    client
        .get(url)
        .send()
        .await
        .context("connect to Ollama")?
        .error_for_status()?
        .json()
        .await
        .context("decode Ollama JSON")
}

fn rate(value: &Value, count: &str, duration: &str) -> Option<f64> {
    let mut count_value = value.get(count)?.as_u64()?;
    if count == "prompt_eval_count" {
        // Prompt time covers only uncached prefill. Older backends do not expose the split.
        count_value = count_value.checked_sub(value.get("prompt_eval_cached_count")?.as_u64()?)?;
    }
    let tokens = f64::from(u32::try_from(count_value).ok()?);
    let seconds = Duration::from_nanos(value.get(duration)?.as_u64()?).as_secs_f64();
    (seconds > 0.0).then_some(tokens / seconds)
}

fn ns_ms(value: Option<u64>) -> Option<f64> {
    value.map(|ns| Duration::from_nanos(ns).as_secs_f64() * 1_000.0)
}

fn needle_prompt() -> String {
    let distractor =
        "The harbor report lists ordinary cargo, calm weather, and routine maintenance. ";
    format!(
        "Read the record and return only the access code. {} The access code is ALPINE-4721. Return only the access code.",
        distractor.repeat(250)
    )
}

fn f64_count(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests;
