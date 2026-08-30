//! Capability-aware benchmarks for every model exposed by an Ollama server.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Completion,
    Tools,
    Vision,
    Audio,
    Thinking,
    Embedding,
    Other,
}

impl Capability {
    fn parse(value: &str) -> Self {
        match value {
            "completion" => Self::Completion,
            "tools" => Self::Tools,
            "vision" => Self::Vision,
            "audio" => Self::Audio,
            "thinking" => Self::Thinking,
            "embedding" => Self::Embedding,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub name: String,
    pub size: u64,
    pub format: String,
    pub family: String,
    pub parameter_size: String,
    pub quantization: String,
    pub capabilities: Vec<Capability>,
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
}

/// Build a fixed, capability-specific case plan using a memory-safe Apple Silicon profile.
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
    let mut cases = Vec::new();
    if has(Capability::Completion) {
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
    if has(Capability::Tools) {
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
    if has(Capability::Vision) {
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
    cases
}

/// Quality-guarded useful cases completed per hour.
#[must_use]
pub fn score_cases(passed: usize, _attempted: usize, elapsed_ms: u64) -> f64 {
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
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_seconds))
        .build()
        .context("build benchmark HTTP client")?;
    let server_version = get_json(
        &client,
        &format!("{}/api/version", config.endpoint.trim_end_matches('/')),
    )
    .await?
    .get("version")
    .and_then(Value::as_str)
    .unwrap_or("unknown")
    .to_owned();
    let models = discover_models(&client, &config.endpoint).await?;
    ensure!(config.trials > 0, "benchmark requires at least one trial");
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
        eprintln!(
            "benchmark model={} capabilities={:?}",
            model.name, model.capabilities
        );
        let plan = benchmark_plan(&model);
        let _ = unload(&client, &config.endpoint, &model.name).await;
        let cold_start = if let Some(case) = plan.first() {
            Some(run_case(&client, &config.endpoint, &model, case).await)
        } else {
            None
        };
        let mut cases = Vec::with_capacity(
            plan.len()
                .saturating_mul(usize::try_from(config.trials).unwrap_or(usize::MAX)),
        );
        for trial in 1..=config.trials {
            for case in &plan {
                let mut result = run_case(&client, &config.endpoint, &model, case).await;
                result.id = format!("{}/trial-{trial}", result.id);
                cases.push(result);
            }
        }
        let (resident_size, resident_vram) = resident(&client, &config.endpoint, &model.name).await;
        let _ = unload(&client, &config.endpoint, &model.name).await;
        results.push(ModelBenchResult {
            metadata: model,
            cold_start,
            cases,
            resident_size,
            resident_vram,
        });
    }
    let rankings = build_rankings(&results);
    Ok(AllModelsReport {
        schema_version: 1,
        endpoint: config.endpoint.clone(),
        server_version,
        profile: "sequential_mac_balanced_v1".to_owned(),
        configuration: BenchmarkConfiguration {
            warm_trials_per_case: config.trials,
            ..BenchmarkConfiguration::default()
        },
        methodology: vec![
            "Models run sequentially to avoid unified-memory contention.".to_owned(),
            format!("One cold-start request is reported but excluded from ranking; each case then runs {} warm trials.", config.trials),
            "Large artifacts use an 8K evaluation context; medium 16K; small 32K, capped by the advertised context.".to_owned(),
            "The deterministic screen uses temperature 0, seed 42, thinking disabled, and a 128-token response-envelope budget.".to_owned(),
            "Capability groups are ranked independently by quality-guarded useful cases/hour.".to_owned(),
            "This is a functional throughput screen, not a broad knowledge or reasoning leaderboard.".to_owned(),
        ],
        models: results,
        rankings,
    })
}

async fn discover_models(client: &Client, endpoint: &str) -> Result<Vec<ModelMetadata>> {
    let tags = get_json(
        client,
        &format!("{}/api/tags", endpoint.trim_end_matches('/')),
    )
    .await?;
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
            .post(format!("{}/api/show", endpoint.trim_end_matches('/')))
            .json(&json!({"model": name}))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let details = show.get("details").cloned().unwrap_or(Value::Null);
        let capabilities = show
            .get("capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(Capability::parse)
            .collect();
        let advertised_context =
            show.get("model_info")
                .and_then(Value::as_object)
                .and_then(|info| {
                    info.iter()
                        .find(|(key, _)| key.ends_with(".context_length"))
                        .and_then(|(_, value)| value.as_u64())
                });
        models.push(ModelMetadata {
            name: name.to_owned(),
            size,
            format: string_field(&details, "format"),
            family: string_field(&details, "family"),
            parameter_size: string_field(&details, "parameter_size"),
            quantization: string_field(&details, "quantization_level"),
            capabilities,
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
    let started = Instant::now();
    let response = match case.kind {
        CaseKind::Embedding => client.post(format!("{}/api/embed", endpoint.trim_end_matches('/')))
            .json(&json!({"model": model.name, "input": ["local model benchmark", "local model benchmark"], "keep_alive": "5m"})).send().await,
        _ => client.post(format!("{}/api/chat", endpoint.trim_end_matches('/')))
            .json(&chat_request(model, case)).send().await,
    };
    let client_ms = millis(started.elapsed());
    let value = match response {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => value,
                Err(error) => return failed(case, client_ms, format!("decode response: {error}")),
            },
            Err(error) => return failed(case, client_ms, format!("HTTP: {error}")),
        },
        Err(error) => return failed(case, client_ms, format!("request: {error}")),
    };
    let output = value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let passed = grade(case.kind, &value, output);
    ModelCaseResult {
        id: case.id.to_owned(),
        capability: case.capability,
        passed,
        client_ms,
        load_ms: ns_ms(value.get("load_duration").and_then(Value::as_u64)),
        prompt_tokens_per_second: rate(&value, "prompt_eval_count", "prompt_eval_duration"),
        decode_tokens_per_second: rate(&value, "eval_count", "eval_duration"),
        prompt_tokens: value.get("prompt_eval_count").and_then(Value::as_u64),
        output_tokens: value.get("eval_count").and_then(Value::as_u64),
        num_ctx: case.num_ctx,
        output_excerpt: (!output.is_empty()).then(|| output.chars().take(160).collect()),
        error: (!passed).then(|| "response did not satisfy the deterministic grader".to_owned()),
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
        CaseKind::Embedding => unreachable!(),
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
    }
}

fn build_rankings(results: &[ModelBenchResult]) -> BTreeMap<Capability, Vec<RankingEntry>> {
    let mut rankings = BTreeMap::new();
    for capability in [
        Capability::Completion,
        Capability::Tools,
        Capability::Vision,
        Capability::Embedding,
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
                    useful_cases_per_hour: score_cases(passed, cases.len(), elapsed),
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

async fn unload(client: &Client, endpoint: &str, model: &str) -> Result<()> {
    client
        .post(format!("{}/api/generate", endpoint.trim_end_matches('/')))
        .json(&json!({"model": model, "prompt": "", "stream": false, "keep_alive": 0}))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn resident(client: &Client, endpoint: &str, model: &str) -> (Option<u64>, Option<u64>) {
    let Ok(value) = get_json(
        client,
        &format!("{}/api/ps", endpoint.trim_end_matches('/')),
    )
    .await
    else {
        return (None, None);
    };
    value
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|entry| {
                entry.get("name").and_then(Value::as_str) == Some(model)
                    || entry.get("model").and_then(Value::as_str) == Some(model)
            })
        })
        .map_or((None, None), |entry| {
            (
                entry.get("size").and_then(Value::as_u64),
                entry.get("size_vram").and_then(Value::as_u64),
            )
        })
}

fn rate(value: &Value, count: &str, duration: &str) -> Option<f64> {
    let tokens = f64::from(u32::try_from(value.get(count)?.as_u64()?).ok()?);
    let seconds = Duration::from_nanos(value.get(duration)?.as_u64()?).as_secs_f64();
    (seconds > 0.0).then_some(tokens / seconds)
}

fn ns_ms(value: Option<u64>) -> Option<f64> {
    value.map(|ns| Duration::from_nanos(ns).as_secs_f64() * 1_000.0)
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
