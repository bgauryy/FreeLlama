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

const OLLAMA_CONFIG_SETTING_NAMES: [&str; 21] = [
    "OLLAMA_DEBUG",
    "OLLAMA_HOST",
    "OLLAMA_CONTEXT_LENGTH",
    "OLLAMA_KEEP_ALIVE",
    "OLLAMA_MAX_LOADED_MODELS",
    "OLLAMA_MAX_TRANSFER_STREAMS",
    "OLLAMA_MAX_QUEUE",
    "OLLAMA_MODELS",
    "OLLAMA_NO_CLOUD",
    "OLLAMA_NOPRUNE",
    "OLLAMA_ORIGINS",
    "OLLAMA_SCHED_SPREAD",
    "OLLAMA_FLASH_ATTENTION",
    "OLLAMA_KV_CACHE_TYPE",
    "OLLAMA_LLM_LIBRARY",
    "OLLAMA_GPU_OVERHEAD",
    "OLLAMA_IGPU_ENABLE",
    "OLLAMA_LOAD_TIMEOUT",
    "LLAMA_ARG_FIT",
    "LLAMA_ARG_FIT_TARGET",
    // `OLLAMA_NUM_PARALLEL` is deliberately last only to keep the list grouped by the help text.
    "OLLAMA_NUM_PARALLEL",
];

pub mod model_bench;
#[cfg(feature = "napi")]
#[allow(unsafe_code)]
pub mod napi;
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

/// Reads an `OLLAMA_*` variable from the launchd per-user domain used by Ollama.app on macOS.
/// Other platforms never call this fallback.
#[cfg(target_os = "macos")]
fn launchctl_getenv(name: &str) -> Option<String> {
    let output = Command::new("launchctl")
        .args(["getenv", name])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Best-effort config visibility without pretending `FreeLlama` can inspect a remote process.
///
/// A colocated service commonly shares its environment with `FreeLlama`, so check this process
/// first on every OS. Ollama.app is launched outside that environment on macOS, where launchd is
/// the only supported extra source available without elevated process inspection.
fn ollama_environment_getenv(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            #[cfg(target_os = "macos")]
            {
                launchctl_getenv(name)
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        })
}

fn ollama_environment_source() -> &'static str {
    if cfg!(target_os = "macos") {
        "best effort: FreeLlama process environment, then macOS launchd session environment; a separately launched Ollama process can still differ"
    } else {
        "best effort: FreeLlama process environment; a separately launched Ollama service or remote endpoint can differ"
    }
}

/// Extract only documented Ollama configuration variables from a `ps eww` command string.
///
/// The command string may contain every environment variable of an application.  Keeping a
/// strict allow-list here is intentional: `doctor` must never turn process inspection into a
/// generic environment dumper.  Values with whitespace are not valid for these settings in the
/// documented service configuration, so whitespace tokenization also prevents a malformed value
/// from swallowing adjacent process data.
#[must_use]
pub fn parse_ollama_process_environment(command: &str) -> BTreeMap<String, String> {
    command
        .split_whitespace()
        .filter_map(|token| token.split_once('='))
        .filter(|(name, value)| {
            OLLAMA_CONFIG_SETTING_NAMES.contains(name) && !value.trim().is_empty()
        })
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn endpoint_is_loopback(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "localhost" || host == "::1" || host.starts_with("127."))
}

/// Inspect the environment of one locally-running Ollama serve process when macOS permits it.
///
/// This is deliberately a *separate* diagnostic from `ollama_environment_getenv`: the latter is
/// a configuration hint, whereas this records what the current user can observe in a process.
/// It is never attempted for a non-loopback endpoint and it reports ambiguity rather than picking
/// a process when more than one candidate is running.  Other platforms return an explicit
/// unsupported status instead of implying their process model is equivalent to macOS.
fn local_ollama_process_environment(endpoint: &str) -> Value {
    if !endpoint_is_loopback(endpoint) {
        return json!({
            "status": "not_attempted",
            "reason": "endpoint is not loopback; local process inspection cannot establish the configuration of a remote service",
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        return json!({
            "status": "unsupported_platform",
            "reason": "same-user Ollama process inspection is currently implemented only for macOS",
        });
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(listing) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
            return json!({ "status": "unavailable", "reason": "could not list same-user processes" });
        };
        if !listing.status.success() {
            return json!({ "status": "unavailable", "reason": "same-user process listing failed" });
        }

        let listing = String::from_utf8_lossy(&listing.stdout);
        let candidates: Vec<&str> = listing
            .lines()
            .filter(|line| {
                line.contains("/ollama") && line.split_whitespace().any(|token| token == "serve")
            })
            .collect();
        if candidates.is_empty() {
            return json!({ "status": "unavailable", "reason": "no same-user Ollama serve process found" });
        }
        if candidates.len() != 1 {
            return json!({
                "status": "ambiguous",
                "reason": "multiple same-user Ollama serve processes found; refusing to attribute one process environment to this endpoint",
                "candidate_count": candidates.len(),
            });
        }

        let Some(pid) = candidates[0].split_whitespace().next() else {
            return json!({ "status": "unavailable", "reason": "could not parse Ollama process id" });
        };
        let Ok(details) = Command::new("ps")
            .args(["eww", "-p", pid, "-o", "command="])
            .output()
        else {
            return json!({ "status": "unavailable", "reason": "could not inspect the observed Ollama process" });
        };
        if !details.status.success() {
            return json!({ "status": "unavailable", "reason": "same-user Ollama process inspection failed" });
        }
        let settings = parse_ollama_process_environment(&String::from_utf8_lossy(&details.stdout));
        let not_observed: Vec<&str> = OLLAMA_CONFIG_SETTING_NAMES
            .iter()
            .copied()
            .filter(|name| !settings.contains_key(*name))
            .collect();
        json!({
            "status": "observed",
            "scope": "one same-user macOS process whose command is ollama serve; endpoint-to-PID association is not provable from ps",
            "pid": pid,
            "settings": settings,
            "not_observed_setting_names": not_observed,
            "note": "Only the 21 documented configuration names are read. A name not observed was not passed in this process environment; it can still have an Ollama default or be configured through another mechanism.",
        })
    }
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
/// Whether that cap fits depends on model sizes, host RAM, accelerator memory, and backend type.
/// The advisory is deliberately machine-neutral; `doctor` reports the local capacity and resident
/// runners separately. Pure function so it is unit-testable without inspecting process state.
#[must_use]
pub fn max_loaded_models_advisory(raw: Option<&str>) -> Option<String> {
    raw.is_none_or(str::is_empty).then(|| {
        "OLLAMA_MAX_LOADED_MODELS is unset, so Ollama picks its own default: the 0 in \
         envconfig/config.go is a sentinel that server/sched.go resolves to 3 x GPU count, i.e. \
         an effective cap of 3 on a single-GPU or CPU-only machine. Capacity is not inferred from \
         that count: three large models may exceed host RAM or accelerator memory, while a large \
         model plus small helpers may fit. Set an explicit value in each Ollama process's service \
         environment after measuring this machine, then restart that process."
            .to_owned()
    })
}

fn find_path_command(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    env::split_paths(&paths)
        .map(|directory| directory.join(&executable))
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

/// Read a timeout from the environment, falling back to `fallback` seconds.
///
/// Single source of truth for the two timeout knobs. `FREELLAMA_CONTROL_TIMEOUT_SECONDS` (30s,
/// small reads of in-memory state) and `FREELLAMA_TASK_TIMEOUT_SECONDS` (900s, a real generation)
/// were each parsed by three separate copies of this function — in `napi.rs`, `platform/mod.rs` and
/// the CLI. All three happened to agree on the defaults, so nothing was broken; but one contract
/// maintained in three places is a divergence waiting to happen, and nothing would have caught it.
///
/// Zero and unparseable values fall back rather than disabling the timeout: a `Client` with no
/// timeout hangs forever against a server that accepts the connection and never answers, which is
/// the failure this exists to prevent.
#[must_use]
pub fn timeout_from_env(name: &str, fallback: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(fallback),
    )
}

/// Default for `FREELLAMA_CONTROL_TIMEOUT_SECONDS` — discovery calls read small in-memory state.
pub const DEFAULT_CONTROL_TIMEOUT_SECS: u64 = 30;
/// Default for `FREELLAMA_TASK_TIMEOUT_SECONDS` — a cold load plus a real generation.
pub const DEFAULT_TASK_TIMEOUT_SECS: u64 = 900;

/// The eleven settings that govern local-model memory, each with the default Ollama actually
/// resolves when the variable is unset.
///
/// Nine are `OLLAMA_*`; the last two are `LLAMA_ARG_FIT*`, which are just as memory-governing and
/// were missed precisely because they do not carry the prefix an auditor greps for.
///
/// Extracted from `doctor` so it can be tested without a live Ollama. That matters more than it
/// sounds: this table has shipped a wrong advisory twice. `OLLAMA_MAX_LOADED_MODELS` was reported
/// as "unlimited" because envconfig declares `0` and only `server/sched.go` resolves that sentinel
/// to `3 x GPU count`; `OLLAMA_FLASH_ATTENTION` was reported as "off" because envconfig's
/// describe-map displays `false`, while the variable is declared with `BoolWithDefault` — whose
/// entire purpose is to let the caller supply the default — and the FAQ states Ollama enables
/// flash attention automatically on supported backends. Both are the same error: reading a
/// declaration and calling it a resolved value.
///
/// `getenv` is injected so tests can drive the table without touching the host environment.
pub fn ollama_env_advisories<F>(getenv: F) -> Value
where
    F: Fn(&str) -> Option<String>,
{
    json!({
        "OLLAMA_MAX_LOADED_MODELS": {
            "value": getenv("OLLAMA_MAX_LOADED_MODELS"),
            "effective_default": "3 x GPU count (envconfig's 0 is a sentinel resolved in server/sched.go)",
        },
        "OLLAMA_NUM_PARALLEL": {
            "value": getenv("OLLAMA_NUM_PARALLEL"),
            "effective_default": "1",
            "note": "Memory scales by OLLAMA_NUM_PARALLEL x context length — raising it multiplies KV-cache memory, it does not just add scheduling slots. Repository benchmark evidence (not a current-host observation) found that a same-backend OLLAMA_NUM_PARALLEL=1 serializes work: SHARED resident admission permits cannot defeat it. Verify actual overlap with your model and host. Separate CPU and GPU Ollama processes can still overlap through FreeLlama's independent backend admission pools. Raise this only for measured same-backend overlap, and qualify any KV-cache change before rollout.",
        },
        "OLLAMA_KEEP_ALIVE": {
            "value": getenv("OLLAMA_KEEP_ALIVE"),
            "effective_default": "5m",
        },
        "OLLAMA_CONTEXT_LENGTH": {
            "value": getenv("OLLAMA_CONTEXT_LENGTH"),
            "effective_default": "4096 tokens (Ollama 0.33.x FAQ; override with OLLAMA_CONTEXT_LENGTH)",
            "note": "The single largest memory lever. FreeLlama's own routing always sends an explicit num_ctx, so tasks routed through `serve` are unaffected — but anything talking to Ollama directly inherits this default.",
        },
        "OLLAMA_KV_CACHE_TYPE": {
            "value": getenv("OLLAMA_KV_CACHE_TYPE"),
            "effective_default": "f16",
            "note": "q8_0 roughly halves KV-cache memory for a given context length; Ollama describes the precision loss as very small, but the quality effect is model and task dependent. It is process-wide, so qualify model quality before rollout. It needs Flash Attention, which is automatic on supported backends. Repository benchmark evidence (not a current-host observation) found prefix reuse can materially reduce warm-prefix latency; validate it on the deployed model before sizing context or cache policy.",
        },
        "OLLAMA_FLASH_ATTENTION": {
            "value": getenv("OLLAMA_FLASH_ATTENTION"),
            "effective_default": "auto — enabled when the backend and device support it (Metal does)",
            "note": "Not off. envconfig declares this with `BoolWithDefault`, whose whole point is that the CALLER supplies the default (plain `Bool` is the one hardcoded to false), and docs/faq.mdx states Ollama 'uses Flash Attention automatically when the selected backend and devices support it'. The `false` visible in envconfig's describe-map is the help-listing display value, not the runtime resolution. This is the same mistake this project already documented for OLLAMA_MAX_LOADED_MODELS: a declaration is not a resolved value. Set 0 to force it off, 1 to force it on.",
        },
        "OLLAMA_MAX_QUEUE": {
            "value": getenv("OLLAMA_MAX_QUEUE"),
            "effective_default": "512",
        },
        "OLLAMA_LOAD_TIMEOUT": {
            "value": getenv("OLLAMA_LOAD_TIMEOUT"),
            "effective_default": "5m",
            "note": "A cold load of a large model can legitimately take minutes; any client timeout below this will give up while Ollama is still working.",
        },
        "OLLAMA_GPU_OVERHEAD": {
            "value": getenv("OLLAMA_GPU_OVERHEAD"),
            "effective_default": "0",
        },
        // Not OLLAMA_-prefixed, and therefore easy to miss when auditing "the OLLAMA_* settings" —
        // but these two govern memory as directly as anything above, so leaving them out made the
        // audit incomplete on exactly the axis it exists to cover.
        "LLAMA_ARG_FIT": {
            "value": getenv("LLAMA_ARG_FIT"),
            "effective_default": "on",
            "note": "llama.cpp automatically fits any memory option you did not set. That is usually what you want, but it means an unset num_ctx or batch size is chosen for you at load time — so a 'why did this model load smaller than I asked' question starts here. Set it off only if you intend to specify every memory option yourself.",
        },
        "LLAMA_ARG_FIT_TARGET": {
            "value": getenv("LLAMA_ARG_FIT_TARGET"),
            "effective_default": "unset — llama.cpp picks its own margin",
            "note": "Target free VRAM margin per device, in MiB, that the automatic fit above aims to leave. On unified memory this is headroom taken from the same pool the OS and everything else uses, which is why Ollama's own loader also refuses a model predicted past 80% of free memory (server/sched.go).",
        },
    })
}

/// Build the version-labelled, categorized Ollama configuration diagnostic.
///
/// `ollama_env_advisories` intentionally remains the backwards-compatible flat view of the eleven
/// memory controls. This report adds the other production-relevant variables advertised by the
/// detected Ollama 0.33 generation, grouped by operational purpose. The detected server version is
/// included because Ollama's environment surface changes over time; this is a known-setting audit,
/// not a claim that every listed variable exists in every past or future release.
///
/// Values come only from the injected best-effort environment reader. No process memory, command
/// line, log, credential store, or unrelated environment variable is inspected.
pub fn ollama_config_diagnostics<F>(server_version: &str, getenv: F) -> Value
where
    F: Fn(&str) -> Option<String>,
{
    let memory_scheduler = ollama_env_advisories(&getenv);
    json!({
        "server_version": server_version,
        "coverage": {
            "known_settings": 21,
            "basis": "production-relevant environment variables advertised by Ollama 0.33.x `ollama serve --help`",
            "version_note": "Ollama adds and removes settings across releases; compare this known-setting audit with the detected version's `ollama serve --help` after an upgrade.",
        },
        "visibility_note": "Best-effort values cannot prove the environment of a separately launched Ollama service or remote endpoint; null means not visible, not necessarily unset.",
        "categories": {
            "memory_scheduler": memory_scheduler,
            "network_security": {
                "OLLAMA_HOST": {
                    "value": getenv("OLLAMA_HOST"),
                    "effective_default": "127.0.0.1:11434 (loopback only)",
                    "note": "Binding to a non-loopback address expands the trust boundary. Put authentication and transport security in front of Ollama before exposing it beyond the host.",
                },
                "OLLAMA_ORIGINS": {
                    "value": getenv("OLLAMA_ORIGINS"),
                    "effective_default": "Ollama's built-in local application origins",
                    "note": "Additional browser origins expand which web pages may call Ollama. Keep this list explicit and narrow; the value is configuration, not an authentication mechanism.",
                },
            },
            "privacy": {
                "OLLAMA_NO_CLOUD": {
                    "value": getenv("OLLAMA_NO_CLOUD"),
                    "effective_default": "false — Ollama cloud features are available",
                    "note": "Set to 1 when the deployment contract requires local-only inference and no Ollama web-search or remote-inference features.",
                },
            },
            "storage_lifecycle": {
                "OLLAMA_MODELS": {
                    "value": getenv("OLLAMA_MODELS"),
                    "effective_default": "$HOME/.ollama/models on macOS/Linux; %USERPROFILE%\\.ollama\\models on Windows",
                    "note": "Controls model-blob placement. Ensure the service account has sufficient disk capacity and permissions; this path may differ from the interactive user's model store.",
                },
                "OLLAMA_NOPRUNE": {
                    "value": getenv("OLLAMA_NOPRUNE"),
                    "effective_default": "false — unused model blobs may be pruned on startup",
                    "note": "Set only when retaining otherwise-unused blobs is intentional and disk growth is monitored.",
                },
            },
            "backend_device": {
                "OLLAMA_LLM_LIBRARY": {
                    "value": getenv("OLLAMA_LLM_LIBRARY"),
                    "effective_default": "auto — detect the best available backend library",
                    "note": "Override only for a diagnosed backend problem or deliberate CPU isolation; request-level num_gpu=0 remains the stronger per-request CPU-routing control.",
                },
                "OLLAMA_SCHED_SPREAD": {
                    "value": getenv("OLLAMA_SCHED_SPREAD"),
                    "effective_default": "false — Ollama chooses model placement",
                    "note": "When enabled, Ollama spreads a model across all GPUs. Leave disabled on single-GPU hosts and measure memory and latency before enabling on multi-GPU hosts.",
                },
                "OLLAMA_IGPU_ENABLE": {
                    "value": getenv("OLLAMA_IGPU_ENABLE"),
                    "effective_default": "false",
                    "note": "Enables integrated-GPU discovery on releases that support this setting. Availability and benefit are hardware- and Ollama-version-dependent.",
                },
            },
            "operations": {
                "OLLAMA_MAX_TRANSFER_STREAMS": {
                    "value": getenv("OLLAMA_MAX_TRANSFER_STREAMS"),
                    "effective_default": "4",
                    "note": "Limits parallel safetensors pull/push transfer streams; it does not control inference concurrency.",
                },
                "OLLAMA_DEBUG": {
                    "value": getenv("OLLAMA_DEBUG"),
                    "effective_default": "false",
                    "note": "Enable temporarily for diagnosis. Debug output can be verbose and should be reviewed under the deployment's log-retention and privacy policy.",
                },
            },
        },
    })
}

fn config_hint(config: &Value, name: &str) -> Option<Value> {
    config["categories"]
        .as_object()?
        .values()
        .find_map(|category| {
            category
                .get(name)
                .and_then(|entry| entry.get("value"))
                .cloned()
        })
}

fn posture_value(
    config: &Value,
    observed_process: &Value,
    name: &str,
) -> (Option<String>, &'static str) {
    if observed_process["status"] == "observed"
        && let Some(value) = observed_process["settings"][name].as_str()
    {
        return (Some(value.to_owned()), "observed_process");
    }
    (
        config_hint(config, name).and_then(|value| value.as_str().map(str::to_owned)),
        "configuration_hint",
    )
}

/// Assess a portable, conservative starting profile without changing an Ollama process.
///
/// This is deliberately not an auto-tuner. The profile contains only settings whose safe starting
/// value does not depend on a particular model, GPU vendor, or benchmark result. A caller still
/// qualifies K/V quantization, expanded context, and same-model parallelism on its own workload.
#[must_use]
pub fn local_conservative_config_posture(config: &Value, observed_process: &Value) -> Value {
    let rule = |name: &str, target: &str, status: &str, note: &str| {
        let (value, source) = posture_value(config, observed_process, name);
        json!({
            "observed_or_hint": value,
            "source": source,
            "target": target,
            "status": status,
            "note": note,
        })
    };
    let (no_cloud, _) = posture_value(config, observed_process, "OLLAMA_NO_CLOUD");
    let (max_loaded, _) = posture_value(config, observed_process, "OLLAMA_MAX_LOADED_MODELS");
    let (parallel, _) = posture_value(config, observed_process, "OLLAMA_NUM_PARALLEL");
    let (queue, _) = posture_value(config, observed_process, "OLLAMA_MAX_QUEUE");
    let (context, _) = posture_value(config, observed_process, "OLLAMA_CONTEXT_LENGTH");
    let (flash, _) = posture_value(config, observed_process, "OLLAMA_FLASH_ATTENTION");
    let (kv_cache, _) = posture_value(config, observed_process, "OLLAMA_KV_CACHE_TYPE");

    let no_cloud_status = if matches!(no_cloud.as_deref(), Some("1" | "true" | "TRUE")) {
        "ready"
    } else {
        "action_required"
    };
    let max_loaded_status = match max_loaded.as_deref() {
        Some("1") => "ready",
        Some(_) => "review",
        None => "recommended",
    };
    let parallel_status = match parallel.as_deref() {
        None | Some("1") => "ready",
        Some(_) => "review",
    };
    let queue_status = match queue.as_deref().and_then(|value| value.parse::<u64>().ok()) {
        Some(value) if (1..=16).contains(&value) => "ready",
        Some(_) => "review",
        None => "recommended",
    };
    let context_status = if context.is_none() { "ready" } else { "review" };
    let flash_status = match flash.as_deref() {
        Some("0") => "review",
        _ => "ready",
    };
    let kv_status = match kv_cache.as_deref() {
        None | Some("f16") => "ready",
        Some("q8_0" | "q4_0") => "qualified_only",
        Some(_) => "review",
    };
    let overall = if no_cloud_status == "action_required" {
        "action_required"
    } else if [
        max_loaded_status,
        parallel_status,
        queue_status,
        context_status,
        flash_status,
        kv_status,
    ]
    .iter()
    .any(|status| *status != "ready")
    {
        "review_required"
    } else {
        "ready"
    };

    json!({
        "profile": "local-conservative-v1",
        "overall": overall,
        "scope": "portable local-only starter profile, not a hardware or quality benchmark result",
        "settings": {
            "OLLAMA_NO_CLOUD": rule("OLLAMA_NO_CLOUD", "1", no_cloud_status, "FreeLlama local-only deployments must disable Ollama cloud features."),
            "OLLAMA_MAX_LOADED_MODELS": rule("OLLAMA_MAX_LOADED_MODELS", "1", max_loaded_status, "Start with one resident model per process; raise only after measured concurrent-fit validation."),
            "OLLAMA_NUM_PARALLEL": rule("OLLAMA_NUM_PARALLEL", "1", parallel_status, "Raise only after validating the multiplied context/KV memory and tail latency."),
            "OLLAMA_MAX_QUEUE": rule("OLLAMA_MAX_QUEUE", "1-16", queue_status, "Keep Ollama's internal backlog finite; FreeLlama admission is a separate queue."),
            "OLLAMA_CONTEXT_LENGTH": rule("OLLAMA_CONTEXT_LENGTH", "unset (Ollama default 4096)", context_status, "Use per-request num_ctx for managed work; qualify any global override."),
            "OLLAMA_FLASH_ATTENTION": rule("OLLAMA_FLASH_ATTENTION", "auto", flash_status, "Ollama enables it automatically on supported backends; forcing it is not a performance claim."),
            "OLLAMA_KV_CACHE_TYPE": rule("OLLAMA_KV_CACHE_TYPE", "f16", kv_status, "q8_0/q4_0 are process-wide quality-versus-memory choices and require workload qualification."),
        },
        "apply": {
            "mutates_nothing": true,
            "macos": "Set approved variables with launchctl setenv, restart Ollama.app, then rerun doctor.",
            "linux": "Set approved variables in the Ollama systemd service environment, restart it, then rerun doctor.",
            "windows": "Set approved variables for the Ollama user service, restart it, then rerun doctor.",
        },
    })
}

/// Reduce macOS `pmset -g therm` output to a narrow, non-sensitive thermal status.
#[must_use]
pub fn parse_macos_thermal_status(output: &str) -> Value {
    if output.contains("No thermal warning level has been recorded") {
        return json!({ "status": "normal" });
    }
    if let Some(level) = output
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("thermal warning level"))
        .and_then(|line| line.rsplit_once(':').map(|(_, value)| value.trim()))
        .filter(|level| !level.is_empty())
    {
        return json!({ "status": "warning", "level": level });
    }
    json!({ "status": "unknown" })
}

fn host_runtime_signals() -> Value {
    #[cfg(target_os = "macos")]
    {
        let thermal = Command::new("pmset")
            .args(["-g", "therm"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map_or_else(
                || json!({ "status": "unavailable" }),
                |output| parse_macos_thermal_status(&String::from_utf8_lossy(&output.stdout)),
            );
        json!({
            "snapshot": "collected during doctor",
            "thermal": { "source": "pmset -g therm", "data": thermal, "permission": "unprivileged" },
            "gpu_memory": { "status": "unavailable", "reason": "macOS does not expose a stable per-process free-VRAM contract for Apple unified memory" },
            "power": { "status": "unavailable", "reason": "powermetrics may provide richer data but requires explicit elevated operator access" },
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        json!({
            "snapshot": "not_collected",
            "thermal": { "status": "unsupported", "reason": "no cross-platform thermal provider is enabled" },
            "gpu_memory": { "status": "unsupported", "reason": "install a vendor-specific telemetry provider; never infer free VRAM from host RAM" },
            "power": { "status": "unsupported", "reason": "no cross-platform power provider is enabled" },
        })
    }
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
    let categorized_config = ollama_config_diagnostics(server_version, ollama_environment_getenv);
    let observed_process_environment = local_ollama_process_environment(endpoint);
    let local_conservative_posture =
        local_conservative_config_posture(&categorized_config, &observed_process_environment);
    let env_config = categorized_config["categories"]["memory_scheduler"].clone();
    // Read the value back out of the table rather than probing the selected environment source a
    // second time, so the advisory and reported value cannot disagree.
    let ollama_env_config_warning =
        max_loaded_models_advisory(env_config["OLLAMA_MAX_LOADED_MODELS"]["value"].as_str());
    // Previously only 3 Ollama variables were reported, and the settings that dominate memory
    // were not among them. Each entry below carries its effective default, because a missing
    // best-effort value can mean "Ollama picks" or that a separately launched service
    // has an environment this process cannot see. Either interpretation differs from "off".
    Ok(json!({
        "endpoint": endpoint,
        "version": version,
        "ollama_cli": cli,
        "running": running,
        "ollama_env_config": env_config,
        // Categorized superset of `ollama_env_config`. Keep the flat field above for clients that
        // already consume it, and use this field for production/security audits.
        "ollama_config": categorized_config,
        // Unlike `ollama_config`, this is not inherited from FreeLlama or launchd. On supported
        // local hosts it is a deliberately allow-listed snapshot of one same-user `ollama serve`
        // process. Keep its scope and endpoint-association limit visible to callers.
        "ollama_process_environment": observed_process_environment,
        // Non-mutating portable posture: operators approve and apply service settings themselves.
        // It is separate from the raw audit so an agent can distinguish "observed" from "safe
        // local-only starter profile" without treating a recommendation as a command.
        "local_conservative_config_posture": local_conservative_posture,
        "host_runtime_signals": host_runtime_signals(),
        // Ollama exposes no endpoint for the server's resolved environment. Report the exact
        // visibility boundary so a null is never mistaken for proof that a separate service (or
        // a remote endpoint) left the setting unset.
        "ollama_env_config_source": ollama_environment_source(),
        "ollama_env_config_warning": ollama_env_config_warning,
        // Local OS discovery — does not need `freellama serve`. MCP used to fetch this from serve
        // and report `machine: null` whenever serve was down, which hid the chip/RAM exactly when
        // you were diagnosing a broken setup.
        "machine": crate::platform::machine_profile(endpoint),
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
