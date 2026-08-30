//! Local Ollama gateway, evidence-aware router, benchmark harness, and server library.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(feature = "napi")]
#[allow(unsafe_code)]
pub mod napi;
pub mod model_bench;
pub mod platform;
pub mod policy;
pub mod proxy;
pub mod recommend;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub schema_version: u32,
    pub name: String,
    pub defaults: Defaults,
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub seed: i64,
    pub temperature: f64,
    pub num_predict: u32,
    pub num_ctx: u32,
    pub timeout_seconds: u64,
    pub target_improvement: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scenario {
    PrefixReuse {
        id: String,
        prefix: String,
        prefix_repetitions: usize,
        turns: Vec<String>,
    },
    DeterministicRestore {
        id: String,
        prompt: String,
        perturbation: String,
    },
    CacheGrowth {
        id: String,
        prefix: String,
        prompt_repetitions: Vec<usize>,
    },
    RunnerReload {
        id: String,
        prompt: String,
        repetitions: usize,
    },
    ModelTransition {
        id: String,
        prompt: String,
        repetitions: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole {
    Primary,
    Secondary,
}

#[derive(Debug, Clone)]
pub struct RequestCase {
    pub id: String,
    pub scenario: String,
    pub model_role: ModelRole,
    pub messages: Vec<Message>,
    pub unload_before: bool,
    pub equivalence_group: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: content.into(),
        }
    }
}

impl Suite {
    /// Load and validate a versioned JSON suite.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, parsed, or uses an unsupported schema.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).with_context(|| format!("read suite {}", path.display()))?;
        let suite: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse suite {}", path.display()))?;
        ensure!(
            suite.schema_version == 1,
            "unsupported suite schema version"
        );
        ensure!(!suite.scenarios.is_empty(), "suite has no scenarios");
        Ok(suite)
    }

    /// Expand declarative scenarios into the exact sequential requests sent to Ollama.
    ///
    /// # Errors
    ///
    /// Returns an error for empty scenarios, invalid repetition counts, or duplicate case IDs.
    pub fn expand(&self) -> Result<Vec<RequestCase>> {
        let mut cases = self
            .scenarios
            .iter()
            .map(expand_scenario)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut ids = BTreeSet::new();
        for case in &cases {
            ensure!(ids.insert(case.id.clone()), "duplicate case id {}", case.id);
        }
        cases.shrink_to_fit();
        Ok(cases)
    }
}

fn expand_scenario(scenario: &Scenario) -> Result<Vec<RequestCase>> {
    match scenario {
        Scenario::PrefixReuse {
            id,
            prefix,
            prefix_repetitions,
            turns,
        } => expand_prefix(id, prefix, *prefix_repetitions, turns),
        Scenario::DeterministicRestore {
            id,
            prompt,
            perturbation,
        } => Ok(expand_restore(id, prompt, perturbation)),
        Scenario::CacheGrowth {
            id,
            prefix,
            prompt_repetitions,
        } => expand_growth(id, prefix, prompt_repetitions),
        Scenario::RunnerReload {
            id,
            prompt,
            repetitions,
        } => expand_reload(id, prompt, *repetitions),
        Scenario::ModelTransition {
            id,
            prompt,
            repetitions,
        } => expand_transition(id, prompt, *repetitions),
    }
}

fn expand_prefix(
    id: &str,
    prefix: &str,
    prefix_repetitions: usize,
    turns: &[String],
) -> Result<Vec<RequestCase>> {
    ensure!(prefix_repetitions > 0, "{id}: prefix_repetitions is zero");
    ensure!(!turns.is_empty(), "{id}: turns is empty");
    let mut cases = Vec::with_capacity(turns.len());
    let mut history = vec![Message::new("system", prefix.repeat(prefix_repetitions))];
    for (index, turn) in turns.iter().enumerate() {
        history.push(Message::new("user", turn));
        cases.push(RequestCase {
            id: format!("{id}/turn-{index}"),
            scenario: id.to_owned(),
            model_role: ModelRole::Primary,
            messages: history.clone(),
            unload_before: false,
            equivalence_group: None,
        });
        history.push(Message::new("assistant", "Acknowledged."));
    }
    Ok(cases)
}

fn expand_restore(id: &str, prompt: &str, perturbation: &str) -> Vec<RequestCase> {
    [
        ("target-a", prompt, Some(format!("{id}/target"))),
        ("perturb", perturbation, None),
        ("target-b", prompt, Some(format!("{id}/target"))),
    ]
    .into_iter()
    .map(|(name, content, group)| RequestCase {
        id: format!("{id}/{name}"),
        scenario: id.to_owned(),
        model_role: ModelRole::Primary,
        messages: vec![Message::new("user", content)],
        unload_before: false,
        equivalence_group: group,
    })
    .collect()
}

fn expand_growth(id: &str, prefix: &str, repetitions: &[usize]) -> Result<Vec<RequestCase>> {
    ensure!(!repetitions.is_empty(), "{id}: no prompt sizes");
    Ok(repetitions
        .iter()
        .map(|count| RequestCase {
            id: format!("{id}/size-{count}"),
            scenario: id.to_owned(),
            model_role: ModelRole::Primary,
            messages: vec![Message::new("user", prefix.repeat(*count))],
            unload_before: false,
            equivalence_group: None,
        })
        .collect())
}

fn expand_reload(id: &str, prompt: &str, repetitions: usize) -> Result<Vec<RequestCase>> {
    ensure!(repetitions > 0, "{id}: repetitions is zero");
    Ok((0..repetitions)
        .map(|index| RequestCase {
            id: format!("{id}/reload-{index}"),
            scenario: id.to_owned(),
            model_role: ModelRole::Primary,
            messages: vec![Message::new("user", prompt)],
            unload_before: true,
            equivalence_group: Some(format!("{id}/reload")),
        })
        .collect())
}

fn expand_transition(id: &str, prompt: &str, repetitions: usize) -> Result<Vec<RequestCase>> {
    ensure!(repetitions > 0, "{id}: repetitions is zero");
    Ok((0..repetitions)
        .flat_map(|index| {
            [
                ("primary", ModelRole::Primary),
                ("secondary", ModelRole::Secondary),
            ]
            .map(move |(name, model_role)| RequestCase {
                id: format!("{id}/{index}-{name}"),
                scenario: id.to_owned(),
                model_role,
                messages: vec![Message::new("user", prompt)],
                unload_before: false,
                equivalence_group: Some(format!("{id}/{name}")),
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub endpoint: String,
    pub primary_model: String,
    pub secondary_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub suite: String,
    pub endpoint: String,
    pub primary_model: String,
    pub secondary_model: Option<String>,
    pub server_version: String,
    pub total_client_ms: u64,
    pub cases: Vec<CaseResult>,
}

impl RunReport {
    #[doc(hidden)]
    #[must_use]
    pub fn fixture(total_client_ms: u64, output_hash: &str) -> Self {
        Self {
            suite: "fixture".to_owned(),
            endpoint: "fixture".to_owned(),
            primary_model: "fixture".to_owned(),
            secondary_model: None,
            server_version: "fixture".to_owned(),
            total_client_ms,
            cases: vec![CaseResult {
                id: "case".to_owned(),
                scenario: "fixture".to_owned(),
                model: "fixture".to_owned(),
                output_hash: Some(output_hash.to_owned()),
                equivalence_group: None,
                client_ms: total_client_ms,
                total_duration_ns: None,
                load_duration_ns: None,
                prompt_eval_count: None,
                prompt_eval_duration_ns: None,
                eval_count: None,
                eval_duration_ns: None,
                resident_size: None,
                resident_vram: None,
                error: None,
            }],
        }
    }

    fn completed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.error.is_none())
            .count()
    }

    fn tasks_per_hour(&self) -> f64 {
        if self.total_client_ms == 0 {
            return 0.0;
        }
        let completed = u32::try_from(self.completed()).unwrap_or(u32::MAX);
        f64::from(completed) * 3_600.0 / Duration::from_millis(self.total_client_ms).as_secs_f64()
    }

    fn stable_equivalence_groups(&self) -> bool {
        let mut groups: BTreeMap<&str, &str> = BTreeMap::new();
        for case in &self.cases {
            let (Some(group), Some(hash)) = (&case.equivalence_group, &case.output_hash) else {
                continue;
            };
            if let Some(previous) = groups.insert(group, hash)
                && previous != hash
            {
                return false;
            }
        }
        true
    }

    fn peak_resident_size(&self) -> Option<u64> {
        self.cases
            .iter()
            .filter_map(|case| case.resident_size)
            .max()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub scenario: String,
    pub model: String,
    pub output_hash: Option<String>,
    pub equivalence_group: Option<String>,
    pub client_ms: u64,
    pub total_duration_ns: Option<u64>,
    pub load_duration_ns: Option<u64>,
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_duration_ns: Option<u64>,
    pub eval_count: Option<u64>,
    pub eval_duration_ns: Option<u64>,
    pub resident_size: Option<u64>,
    pub resident_vram: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub baseline: RunReport,
    pub candidate: RunReport,
    pub baseline_tasks_per_hour: f64,
    pub candidate_tasks_per_hour: f64,
    pub improvement: f64,
    pub guardrails: Guardrails,
    pub baseline_peak_resident: Option<u64>,
    pub candidate_peak_resident: Option<u64>,
    pub target_improvement: f64,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guardrails {
    pub exact_outputs: GuardrailStatus,
    pub deterministic_restore: GuardrailStatus,
    pub all_candidate_cases_completed: GuardrailStatus,
    pub memory: GuardrailStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GuardrailStatus {
    Pass,
    Fail,
}

impl From<bool> for GuardrailStatus {
    fn from(value: bool) -> Self {
        if value { Self::Pass } else { Self::Fail }
    }
}

/// Compare isolated stock and candidate reports under the frozen KPI contract.
///
/// # Errors
///
/// Returns an error when the reports do not contain the same number of cases.
pub fn compare(
    baseline: &RunReport,
    candidate: &RunReport,
    target_improvement: f64,
) -> Result<Comparison> {
    ensure!(
        baseline.cases.len() == candidate.cases.len(),
        "baseline and candidate case counts differ"
    );
    let candidate_by_id: BTreeMap<_, _> = candidate
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect();
    let exact_outputs = baseline.cases.iter().all(|left| {
        candidate_by_id.get(left.id.as_str()).is_some_and(|right| {
            left.output_hash.is_some() && left.output_hash == right.output_hash
        })
    });
    let baseline_tasks_per_hour = baseline.tasks_per_hour();
    let candidate_tasks_per_hour = candidate.tasks_per_hour();
    let improvement = if baseline_tasks_per_hour == 0.0 {
        0.0
    } else {
        candidate_tasks_per_hour / baseline_tasks_per_hour - 1.0
    };
    let deterministic_restore =
        baseline.stable_equivalence_groups() && candidate.stable_equivalence_groups();
    let all_candidate_cases_completed = candidate.completed() == candidate.cases.len();
    let baseline_peak_resident = baseline.peak_resident_size();
    let candidate_peak_resident = candidate.peak_resident_size();
    let memory_guard = match (baseline_peak_resident, candidate_peak_resident) {
        (Some(stock), Some(patched)) => patched <= stock.saturating_add(stock / 20),
        (None, None) => true,
        _ => false,
    };
    let verdict = if improvement >= target_improvement
        && exact_outputs
        && deterministic_restore
        && all_candidate_cases_completed
        && memory_guard
    {
        Verdict::Accept
    } else {
        Verdict::Reject
    };

    Ok(Comparison {
        baseline: baseline.clone(),
        candidate: candidate.clone(),
        baseline_tasks_per_hour,
        candidate_tasks_per_hour,
        improvement,
        guardrails: Guardrails {
            exact_outputs: exact_outputs.into(),
            deterministic_restore: deterministic_restore.into(),
            all_candidate_cases_completed: all_candidate_cases_completed.into(),
            memory: memory_guard.into(),
        },
        baseline_peak_resident,
        candidate_peak_resident,
        target_improvement,
        verdict,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OllamaCliDiagnostic {
    pub server_version: String,
    pub client_version: Option<String>,
    pub reported_version: Option<String>,
    pub matches_server: bool,
    pub warning: Option<String>,
    pub invoked_path: Option<String>,
    pub resolved_path: Option<String>,
}

/// Parse `ollama --version` output against the active server version.
#[must_use]
pub fn parse_ollama_cli_version(
    server_version: &str,
    stdout: &str,
    stderr: &str,
) -> OllamaCliDiagnostic {
    let reported_version = stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("ollama version is ")
            .map(str::to_owned)
    });
    let client_version = stderr
        .lines()
        .chain(stdout.lines())
        .find_map(|line| {
            line.trim()
                .strip_prefix("Warning: client version is ")
                .map(str::to_owned)
        })
        .or_else(|| reported_version.clone());
    let matches_server = client_version.as_deref() == Some(server_version);
    let warning = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| line.starts_with("Warning: client version is "))
        .map(str::to_owned);
    OllamaCliDiagnostic {
        server_version: server_version.to_owned(),
        client_version,
        reported_version,
        matches_server,
        warning,
        invoked_path: None,
        resolved_path: None,
    }
}

/// Reads an `OLLAMA_*` variable from the launchd per-user domain — the environment the macOS
/// Ollama app actually runs under (`launchctl setenv`, not a shell export), which is why
/// `std::env::var` here would read the wrong process's environment entirely.
fn launchctl_getenv(name: &str) -> Option<String> {
    let output = Command::new("launchctl").args(["getenv", name]).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Advise when `OLLAMA_MAX_LOADED_MODELS` is unset.
///
/// The `0` that `envconfig/config.go` declares as this variable's default is a *sentinel*, not an
/// effective value: `server/sched.go` resolves it at load time to `defaultModelsPerGPU * gpu_count`
/// — `defaultModelsPerGPU` is 3, so a single-GPU Mac gets an effective cap of **3**, not
/// "unlimited". (An earlier version of this advisory said unlimited, having read the envconfig
/// default without following the sentinel through the scheduler. Ollama's own FAQ states the 3
/// directly.)
///
/// A cap of 3 is still far too many for large models on unified memory: 3 x ~22GB does not fit in
/// 48GB, which is the condition that let this project co-resident two large models and crash the
/// server (see `skills/freellama/references/model-selection.md`). The advisory stands; only its
/// stated reason needed correcting. Pure function so it's unit-testable without shelling out to
/// `launchctl`.
#[must_use]
pub fn max_loaded_models_advisory(raw: Option<&str>) -> Option<String> {
    raw.is_none_or(str::is_empty).then(|| {
        "OLLAMA_MAX_LOADED_MODELS is unset, so Ollama picks its own default: the 0 in \
         envconfig/config.go is a sentinel that server/sched.go resolves to 3 x GPU count, i.e. \
         an effective cap of 3 on a single-GPU machine. That is a cap, but not a useful one for \
         large models on unified memory — 3 x ~22GB does not fit in 48GB. Set it with \
         `launchctl setenv OLLAMA_MAX_LOADED_MODELS 1` (then restart the Ollama app) if this \
         machine should never co-resident two large models."
            .to_owned()
    })
}

fn find_path_command(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn ollama_cli_output(executable: &Path) -> std::io::Result<(String, String)> {
    let output = Command::new(executable).arg("--version").output()?;
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if cfg!(target_os = "macos")
        && let Ok(pty) = Command::new("script")
            .arg("-q")
            .arg("/dev/null")
            .arg(executable)
            .arg("--version")
            .output()
    {
        stdout.push_str(&String::from_utf8_lossy(&pty.stdout));
        stderr.push_str(&String::from_utf8_lossy(&pty.stderr));
    }
    Ok((stdout, stderr))
}

/// Query the diagnostic endpoints required by the harness.
///
/// # Errors
///
/// Returns an error when Ollama is unavailable or returns invalid diagnostic JSON.
pub async fn doctor(endpoint: &str) -> Result<Value> {
    // `reqwest::Client` applies no request timeout unless asked. `doctor` is the tool an agent is
    // told to reach for when something else is already timing out, so it is the last one that
    // should be able to hang against a wedged Ollama.
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build Ollama HTTP client")?;
    let version: Value = client
        .get(url(endpoint, "/api/version"))
        .send()
        .await
        .context("connect to Ollama")?
        .error_for_status()
        .context("Ollama version request")?
        .json()
        .await
        .context("decode Ollama version")?;
    let running: Value = client
        .get(url(endpoint, "/api/ps"))
        .send()
        .await
        .context("query Ollama runners")?
        .error_for_status()
        .context("Ollama process request")?
        .json()
        .await
        .context("decode Ollama processes")?;
    let server_version = version
        .get("version")
        .and_then(Value::as_str)
        .context("Ollama version response has no version")?;
    let cli = find_path_command("ollama").and_then(|invoked_path| {
        let resolved_path = invoked_path.canonicalize().ok();
        let (stdout, stderr) = ollama_cli_output(&invoked_path).ok()?;
        let mut diagnostic = parse_ollama_cli_version(server_version, &stdout, &stderr);
        diagnostic.invoked_path = Some(invoked_path.display().to_string());
        diagnostic.resolved_path = resolved_path.map(|path| path.display().to_string());
        Some(diagnostic)
    });
    let ollama_max_loaded_models = launchctl_getenv("OLLAMA_MAX_LOADED_MODELS");
    let ollama_env_config_warning = max_loaded_models_advisory(ollama_max_loaded_models.as_deref());
    // Previously only 3 of Ollama's 16 `OLLAMA_*` variables were reported, and the three that
    // dominate memory were not among them. Each entry below carries its effective default, because
    // `launchctl getenv` returning empty means "Ollama picks" — which is not the same as "off",
    // and reporting a bare null invites exactly the misreading that produced the wrong
    // MAX_LOADED_MODELS advisory above.
    let env_config = json!({
        "OLLAMA_MAX_LOADED_MODELS": {
            "value": ollama_max_loaded_models,
            "effective_default": "3 x GPU count (envconfig's 0 is a sentinel resolved in server/sched.go)",
        },
        "OLLAMA_NUM_PARALLEL": {
            "value": launchctl_getenv("OLLAMA_NUM_PARALLEL"),
            "effective_default": "1",
            "note": "Memory scales by OLLAMA_NUM_PARALLEL x context length — raising it multiplies KV-cache memory, it does not just add scheduling slots.",
        },
        "OLLAMA_KEEP_ALIVE": {
            "value": launchctl_getenv("OLLAMA_KEEP_ALIVE"),
            "effective_default": "5m",
        },
        "OLLAMA_CONTEXT_LENGTH": {
            "value": launchctl_getenv("OLLAMA_CONTEXT_LENGTH"),
            "effective_default": "VRAM-tiered: 4k under 24GiB, 32k for 24-48GiB, 256k at 48GiB+",
            "note": "The single largest memory lever. FreeLlama's own routing always sends an explicit num_ctx, so tasks routed through `serve` are unaffected — but anything talking to Ollama directly inherits this default.",
        },
        "OLLAMA_KV_CACHE_TYPE": {
            "value": launchctl_getenv("OLLAMA_KV_CACHE_TYPE"),
            "effective_default": "f16",
            "note": "q8_0 roughly halves KV-cache memory for a given context length; requires OLLAMA_FLASH_ATTENTION.",
        },
        "OLLAMA_FLASH_ATTENTION": {
            "value": launchctl_getenv("OLLAMA_FLASH_ATTENTION"),
            "effective_default": "off",
        },
        "OLLAMA_MAX_QUEUE": {
            "value": launchctl_getenv("OLLAMA_MAX_QUEUE"),
            "effective_default": "512",
        },
        "OLLAMA_LOAD_TIMEOUT": {
            "value": launchctl_getenv("OLLAMA_LOAD_TIMEOUT"),
            "effective_default": "5m",
            "note": "A cold load of a large model can legitimately take minutes; any client timeout below this will give up while Ollama is still working.",
        },
        "OLLAMA_GPU_OVERHEAD": {
            "value": launchctl_getenv("OLLAMA_GPU_OVERHEAD"),
            "effective_default": "0",
        },
    });
    Ok(json!({
        "endpoint": endpoint,
        "version": version,
        "ollama_cli": cli,
        "running": running,
        "ollama_env_config": env_config,
        // Stated rather than implied: `launchctl getenv` reads the launchd session environment,
        // which is what the Ollama.app inherits. A server started from a shell
        // (`OLLAMA_CONTEXT_LENGTH=64000 ollama serve`) has its variables in that process's
        // environment only, where launchctl cannot see them — verified. A `null` value below
        // therefore means "not set via launchd", not "definitely unset". Ollama exposes no
        // endpoint reporting its own effective config, so this is a real limit, not an oversight.
        "ollama_env_config_source": "launchctl getenv (launchd session env, i.e. what Ollama.app inherits). A server launched from a shell with inline env vars will show null here even when the values are set in its own process environment.",
        "ollama_env_config_warning": ollama_env_config_warning,
    }))
}

/// Run one frozen suite against one Ollama endpoint.
///
/// # Errors
///
/// Returns an error for invalid configuration or when the server cannot be initialized.
pub async fn run_suite(suite: &Suite, config: &RunConfig) -> Result<RunReport> {
    let cases = suite.expand()?;
    if cases
        .iter()
        .any(|case| case.model_role == ModelRole::Secondary)
    {
        ensure!(
            config.secondary_model.is_some(),
            "suite requires --secondary-model for transition cases"
        );
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(suite.defaults.timeout_seconds))
        .build()
        .context("build HTTP client")?;
    let version = server_version(&client, &config.endpoint).await?;
    unload(&client, &config.endpoint, &config.primary_model).await?;
    if let Some(model) = &config.secondary_model {
        unload(&client, &config.endpoint, model).await?;
    }

    let run_start = Instant::now();
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let model = match case.model_role {
            ModelRole::Primary => &config.primary_model,
            ModelRole::Secondary => config
                .secondary_model
                .as_ref()
                .context("transition case requires a secondary model")?,
        };
        if case.unload_before {
            unload(&client, &config.endpoint, model).await?;
        }
        results.push(run_case(&client, suite, config, &case, model).await);
    }

    Ok(RunReport {
        suite: suite.name.clone(),
        endpoint: config.endpoint.clone(),
        primary_model: config.primary_model.clone(),
        secondary_model: config.secondary_model.clone(),
        server_version: version,
        total_client_ms: millis(run_start.elapsed()),
        cases: results,
    })
}

async fn run_case(
    client: &Client,
    suite: &Suite,
    config: &RunConfig,
    case: &RequestCase,
    model: &str,
) -> CaseResult {
    let started = Instant::now();
    let response = client
        .post(url(&config.endpoint, "/api/chat"))
        .json(&json!({
            "model": model,
            "messages": case.messages,
            "stream": false,
            "options": {
                "temperature": suite.defaults.temperature,
                "seed": suite.defaults.seed,
                "num_predict": suite.defaults.num_predict,
                "num_ctx": suite.defaults.num_ctx
            }
        }))
        .send()
        .await;
    let client_ms = millis(started.elapsed());

    let value = match response {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => value,
                Err(error) => return failed(case, model, client_ms, format!("decode: {error}")),
            },
            Err(error) => return failed(case, model, client_ms, format!("HTTP: {error}")),
        },
        Err(error) => return failed(case, model, client_ms, format!("request: {error}")),
    };

    if value.get("done").and_then(Value::as_bool) != Some(true)
        || !value.get("message").is_some_and(Value::is_object)
    {
        return failed(
            case,
            model,
            client_ms,
            "response is not a terminal Ollama chat message".to_owned(),
        );
    }

    let content = value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let thinking = value
        .pointer("/message/thinking")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update([0]);
    hasher.update(thinking.as_bytes());
    let output_hash = format!("{:x}", hasher.finalize());
    let (resident_size, resident_vram) = resident(client, &config.endpoint, model).await;

    CaseResult {
        id: case.id.clone(),
        scenario: case.scenario.clone(),
        model: model.to_owned(),
        output_hash: Some(output_hash),
        equivalence_group: case.equivalence_group.clone(),
        client_ms,
        total_duration_ns: value.get("total_duration").and_then(Value::as_u64),
        load_duration_ns: value.get("load_duration").and_then(Value::as_u64),
        prompt_eval_count: value.get("prompt_eval_count").and_then(Value::as_u64),
        prompt_eval_duration_ns: value.get("prompt_eval_duration").and_then(Value::as_u64),
        eval_count: value.get("eval_count").and_then(Value::as_u64),
        eval_duration_ns: value.get("eval_duration").and_then(Value::as_u64),
        resident_size,
        resident_vram,
        error: None,
    }
}

fn failed(case: &RequestCase, model: &str, client_ms: u64, error: String) -> CaseResult {
    CaseResult {
        id: case.id.clone(),
        scenario: case.scenario.clone(),
        model: model.to_owned(),
        output_hash: None,
        equivalence_group: case.equivalence_group.clone(),
        client_ms,
        total_duration_ns: None,
        load_duration_ns: None,
        prompt_eval_count: None,
        prompt_eval_duration_ns: None,
        eval_count: None,
        eval_duration_ns: None,
        resident_size: None,
        resident_vram: None,
        error: Some(error),
    }
}

async fn server_version(client: &Client, endpoint: &str) -> Result<String> {
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

async fn unload(client: &Client, endpoint: &str, model: &str) -> Result<()> {
    client
        .post(url(endpoint, "/api/generate"))
        .json(&json!({ "model": model, "prompt": "", "keep_alive": 0, "stream": false }))
        .send()
        .await
        .with_context(|| format!("request unload for {model}"))?
        .error_for_status()
        .with_context(|| format!("Ollama rejected unload for {model}"))?;
    Ok(())
}

async fn resident(client: &Client, endpoint: &str, model: &str) -> (Option<u64>, Option<u64>) {
    let Ok(response) = client.get(url(endpoint, "/api/ps")).send().await else {
        return (None, None);
    };
    let Ok(value) = response.json::<Value>().await else {
        return (None, None);
    };
    let Some(models) = value.get("models").and_then(Value::as_array) else {
        return (None, None);
    };
    models
        .iter()
        .find(|entry| {
            entry.get("name").and_then(Value::as_str) == Some(model)
                || entry.get("model").and_then(Value::as_str) == Some(model)
        })
        .map_or((None, None), |entry| {
            (
                entry.get("size").and_then(Value::as_u64),
                entry.get("size_vram").and_then(Value::as_u64),
            )
        })
}

fn url(endpoint: &str, path: &str) -> String {
    format!("{}{path}", endpoint.trim_end_matches('/'))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Write a stable, human-readable JSON result artifact.
///
/// # Errors
///
/// Returns an error when serialization or filesystem operations fail.
pub fn write_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(value).context("encode report")?;
    fs::write(path, bytes).with_context(|| format!("write report {}", path.display()))
}

/// Require state-isolated servers for a causal comparison.
///
/// # Errors
///
/// Returns an error when both endpoint strings resolve to the same normalized URL.
pub fn validate_endpoints(baseline: &str, candidate: &str) -> Result<()> {
    if baseline.trim_end_matches('/') == candidate.trim_end_matches('/') {
        bail!("baseline and candidate must be isolated Ollama servers on different endpoints");
    }
    Ok(())
}
