//! Ollama server, CLI and host diagnostics behind `freellama doctor`.

use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::ollama_http::{server_version, url};
use crate::platform::resources::run_bounded;
use crate::{DEFAULT_CONTROL_TIMEOUT_SECS, timeout_from_env};

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

/// Best-effort config visibility without pretending `FreeLlama` can inspect a remote process.
///
/// A colocated service commonly shares its environment with `FreeLlama`, so check this process
/// first on every OS. Ollama.app is launched outside that environment on macOS, where launchd is
/// the only supported extra source available without elevated process inspection.
fn ollama_environment_getenv(name: &str) -> Option<String> {
    process_getenv(name).or_else(|| service_manager_getenv(name))
}

fn process_getenv(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Reads an `OLLAMA_*` variable from the launchd per-user domain used by Ollama.app on macOS,
/// through the bounded `launchctl getenv` probe. Blocking: call it off the async runtime.
#[cfg(target_os = "macos")]
fn service_manager_getenv(name: &str) -> Option<String> {
    crate::platform::ollama_env::launchd_getenv(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn service_manager_getenv(_name: &str) -> Option<String> {
    None
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

/// Inspect the environment of one locally-running Ollama serve process when macOS permits it.
///
/// This is deliberately a *separate* diagnostic from `ollama_environment_getenv`: the latter is
/// a configuration hint, whereas this records what the current user can observe in a process.
/// It is never attempted for a non-loopback endpoint and it reports ambiguity rather than picking
/// a process when more than one candidate is running.  Other platforms return an explicit
/// unsupported status instead of implying their process model is equivalent to macOS.
fn local_ollama_process_environment(endpoint: &str) -> Value {
    if !crate::net::is_loopback_url(endpoint) {
        return json!({
            "status": "not_attempted",
            "reason": "endpoint is not loopback; local process inspection cannot establish the configuration of a remote service",
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        json!({
            "status": "unsupported_platform",
            "reason": "same-user Ollama process inspection is currently implemented only for macOS",
        })
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(listing) = run_bounded(
            Command::new("ps").args(["-axo", "pid=,command="]),
            PROCESS_PROBE_TIMEOUT,
            PROCESS_LISTING_CAP,
            false,
        ) else {
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
        let Ok(details) = run_bounded(
            Command::new("ps").args(["eww", "-p", pid, "-o", "command="]),
            PROCESS_PROBE_TIMEOUT,
            PROCESS_LISTING_CAP,
            false,
        ) else {
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
/// "unlimited". Ollama's own FAQ states the 3 directly.
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

/// Deadline for one `ollama --version` run; the enclosing doctor probe has its own outer bound.
const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// Deadline for one `ps` run.
#[cfg(target_os = "macos")]
const PROCESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// `ollama --version` prints one or two short lines.
const CLI_OUTPUT_CAP: u64 = 64 * 1024;
/// A full process table with command lines, or one process environment, on a busy host.
#[cfg(target_os = "macos")]
const PROCESS_LISTING_CAP: u64 = 4 * 1024 * 1024;

fn ollama_cli_output(executable: &Path) -> std::io::Result<(String, String)> {
    let output = run_bounded(
        Command::new(executable).arg("--version"),
        CLI_PROBE_TIMEOUT,
        CLI_OUTPUT_CAP,
        true,
    )?;
    let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if cfg!(target_os = "macos")
        && let Ok(pty) = run_bounded(
            Command::new("script")
                .arg("-q")
                .arg("/dev/null")
                .arg(executable)
                .arg("--version"),
            CLI_PROBE_TIMEOUT,
            CLI_OUTPUT_CAP,
            true,
        )
    {
        stdout.push_str(&String::from_utf8_lossy(&pty.stdout));
        stderr.push_str(&String::from_utf8_lossy(&pty.stderr));
    }
    Ok((stdout, stderr))
}

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
            "effective_default": "documentation drift: current context docs use VRAM-tiered 4k/32k/256k; FAQ still says 4096",
            "note": "The single largest memory lever. FreeLlama's managed routing sends an explicit num_ctx. For direct Ollama work, do not infer a tier from host RAM: inspect the loaded runner's context_length through /api/ps.",
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
        // Not OLLAMA_-prefixed, but these two govern memory as directly as anything above.
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
            "known_settings": OLLAMA_CONFIG_SETTING_NAMES.len(),
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
            "OLLAMA_CONTEXT_LENGTH": rule("OLLAMA_CONTEXT_LENGTH", "unset; observe the VRAM-tiered runtime default with /api/ps", context_status, "Use per-request num_ctx for managed work; current upstream pages disagree on a fixed default, so qualify any global override and inspect the loaded runner."),
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
    let recorded_level = output.lines().find_map(|line| {
        let (key, value) = line.rsplit_once([':', '='])?;
        let key = key.trim().to_ascii_lowercase();
        matches!(
            key.as_str(),
            "thermal warning level" | "note: thermal warning level"
        )
        .then(|| value.trim().parse::<u32>().ok())
        .flatten()
    });
    if let Some(level) = recorded_level {
        return if level == 0 {
            json!({ "status": "normal" })
        } else {
            json!({ "status": "warning", "level": level.to_string() })
        };
    }
    let reason = if output.contains("No thermal warning level has been recorded") {
        "thermal_warning_not_recorded"
    } else {
        "thermal_warning_unavailable_or_invalid"
    };
    json!({ "status": "unknown", "reason": reason })
}

fn host_runtime_signals() -> Value {
    #[cfg(target_os = "macos")]
    {
        let thermal = crate::platform::resources::bounded_command_with(
            "pmset",
            &["-g", "therm"],
            Duration::from_millis(500),
        )
        .map_or_else(
            || json!({ "status": "unavailable" }),
            |output| parse_macos_thermal_status(&output),
        );
        // Apple silicon records no pmset thermal warning; the system thermal pressure level is
        // the unprivileged signal there (`OSThermalNotification.h`).
        let mut pressure_sample = crate::platform::resources::HostResources::default();
        if let Some(output) = crate::platform::resources::bounded_command_with(
            "/usr/bin/notifyutil",
            &["-g", "com.apple.system.thermalpressurelevel"],
            Duration::from_millis(500),
        ) {
            crate::platform::resources::apply_thermal_pressure(&mut pressure_sample, &output);
        }
        json!({
            "snapshot": "collected during doctor",
            "thermal": { "source": "pmset -g therm", "data": thermal, "permission": "unprivileged" },
            "thermal_pressure": {
                "source": "notifyutil com.apple.system.thermalpressurelevel",
                "level": pressure_sample.thermal_pressure,
                "throttled": pressure_sample.thermal_throttled,
                "permission": "unprivileged",
            },
            "gpu_memory": { "status": "unavailable", "reason": "macOS does not expose a stable per-process free-VRAM contract for Apple unified memory; status reports the driver's allocated bytes in gpu_activity" },
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

const DOCTOR_LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Query the diagnostic endpoints required by the harness.
///
/// # Errors
///
/// Returns an error when Ollama is unavailable or returns invalid diagnostic JSON.
pub async fn doctor(endpoint: &str) -> Result<Value> {
    // `reqwest::Client` applies no request timeout unless asked. `doctor` is the tool an agent is
    // told to reach for when something else is already timing out, so it is the last one that
    // should be able to hang against a wedged Ollama.
    let client = crate::net::build_http_client(|builder| {
        builder.timeout(timeout_from_env(
            "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
            DEFAULT_CONTROL_TIMEOUT_SECS,
        ))
    })
    .context("build Ollama HTTP client")?;
    let server_version = server_version(&client, endpoint).await?;
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
    // `ollama --version` and `ps` are synchronous subprocesses. Run them off the async runtime
    // and bound them: a wedged CLI must not hang the tool meant for diagnosing hangs.
    let local = {
        let (server_version, endpoint) = (server_version.clone(), endpoint.to_owned());
        tokio::task::spawn_blocking(move || {
            let cli = find_path_command("ollama").and_then(|invoked_path| {
                let resolved_path = invoked_path.canonicalize().ok();
                let (stdout, stderr) = ollama_cli_output(&invoked_path).ok()?;
                let mut diagnostic = parse_ollama_cli_version(&server_version, &stdout, &stderr);
                diagnostic.invoked_path = Some(invoked_path.display().to_string());
                diagnostic.resolved_path = resolved_path.map(|path| path.display().to_string());
                Some(diagnostic)
            });
            (cli, local_ollama_process_environment(&endpoint))
        })
    };
    let (cli, observed_process_environment) = match tokio::time::timeout(
        DOCTOR_LOCAL_PROBE_TIMEOUT,
        local,
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => (
            None,
            json!({"status": "unavailable", "reason": format!("local process probes failed: {join_error}")}),
        ),
        Err(_) => (
            None,
            json!({"status": "unavailable", "reason": "local process probes timed out"}),
        ),
    };
    // `launchctl getenv` (one per setting), `pmset` and `df` are individually bounded blocking
    // subprocesses; run them off the async runtime too.
    let host = {
        let (server_version, endpoint) = (server_version.clone(), endpoint.to_owned());
        tokio::task::spawn_blocking(move || {
            (
                ollama_config_diagnostics(&server_version, ollama_environment_getenv),
                host_runtime_signals(),
                crate::platform::machine_profile(&endpoint),
            )
        })
        .await
    };
    let server_version = server_version.as_str();
    let (categorized_config, host_runtime_signals, machine) = match host {
        Ok(result) => result,
        Err(join_error) => (
            ollama_config_diagnostics(server_version, process_getenv),
            json!({"status": "unavailable", "reason": format!("host probes failed: {join_error}")}),
            crate::platform::machine_profile_off_runtime(endpoint).await,
        ),
    };
    let local_conservative_posture =
        local_conservative_config_posture(&categorized_config, &observed_process_environment);
    let env_config = categorized_config["categories"]["memory_scheduler"].clone();
    // Read the value back out of the table rather than probing the selected environment source a
    // second time, so the advisory and reported value cannot disagree.
    let ollama_env_config_warning =
        max_loaded_models_advisory(env_config["OLLAMA_MAX_LOADED_MODELS"]["value"].as_str());
    // Each reported setting carries its effective default, because a missing best-effort value
    // can mean "Ollama picks" or that a separately launched service has an environment this
    // process cannot see. Either interpretation differs from "off".
    Ok(json!({
        "endpoint": endpoint,
        "version": {"version": server_version},
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
        "host_runtime_signals": host_runtime_signals,
        // Ollama exposes no endpoint for the server's resolved environment. Report the exact
        // visibility boundary so a null is never mistaken for proof that a separate service (or
        // a remote endpoint) left the setting unset.
        "ollama_env_config_source": ollama_environment_source(),
        "ollama_env_config_warning": ollama_env_config_warning,
        // Local OS discovery that works without `freellama serve`, so chip and RAM stay visible
        // while diagnosing a broken setup.
        "machine": machine,
    }))
}
