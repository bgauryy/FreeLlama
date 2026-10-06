//! `freellama serve`: operator file discovery, platform configuration and startup.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use freellama::platform::{PlatformConfig, serve as serve_platform};

use super::args::{AdmissionArgs, PlatformStartArgs, ProductionArgs};

/// Operator evidence files for `serve`: flag, then environment, then the working directory, then
/// the per-user config directory.
///
/// `minConfidence: "medium"` needs BOTH a policy file and a benchmark report, and requiring two
/// explicit flags meant almost nobody ever had them — the gate degraded to refusing everything.
/// Fall back to conventional paths so the common case works (including an MCP-autostarted
/// `serve`, which has no flags), and say which files were picked up so it is never silent.
fn resolve_operator_files(
    policy_file: Option<PathBuf>,
    benchmark_report: Option<PathBuf>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let policy_file = policy_file.or_else(|| discover_config("platform.toml"));
    let benchmark_report = benchmark_report.or_else(|| discover_config("benchmark-report.json"));
    for (label, path) in [
        ("policy file", &policy_file),
        ("benchmark report", &benchmark_report),
    ] {
        if let Some(path) = path {
            eprintln!("freellama: using {label} {}", path.display());
        }
    }
    if policy_file.is_none() || benchmark_report.is_none() {
        eprintln!(
            "freellama: note — `minConfidence: \"medium\"` needs both a policy file and a \
             benchmark report; every route will grade `low` until both exist.\n\
             freellama: generate a policy with `python3 \
             benchmark/harness/scripts/policy_from_eval.py --aggregate <harness aggregate.json> \
             --task <task> --out platform.toml`"
        );
    }
    (policy_file, benchmark_report)
}

pub(crate) async fn start_platform(args: PlatformStartArgs) -> Result<()> {
    let (policy_file, benchmark_report) =
        resolve_operator_files(args.policy_file, args.benchmark_report);
    let mut config = PlatformConfig::new(
        args.listen,
        args.backends.upstream,
        benchmark_report,
        policy_file,
    );
    configure_resource_policy(&mut config, &args.admission)?;
    if let Some(cpu_upstream) = args.backends.cpu_upstream {
        eprintln!(
            "freellama: assigning {} model(s) to CPU Ollama at {cpu_upstream}",
            args.backends.cpu_model.len()
        );
        config = config.with_cpu_backend(cpu_upstream, args.backends.cpu_model);
    }
    if let Some(slots) = args.admission.max_concurrent_tasks {
        config = config.with_max_concurrent_tasks(slots);
    }
    if let Some(slots) = args.admission.cpu_max_concurrent_tasks {
        config = config.with_cpu_max_concurrent_tasks(slots);
    }
    if let Some(seconds) = args.admission.max_queue_wait_seconds {
        config = config.with_max_queue_wait(Duration::from_secs(seconds));
    }
    if let Some(max) = args.admission.max_queued_tasks {
        config = config.with_max_queued_tasks(max);
    }
    if let Some(max) = args.admission.cpu_max_queued_tasks {
        config = config.with_cpu_max_queued_tasks(max);
    }
    if let Some(max) = args.admission.raw_proxy_max_concurrent_requests {
        config = config.with_raw_proxy_max_concurrent_requests(max);
    }
    if let Some(seconds) = args.admission.raw_queue_wait_seconds {
        config = config.with_raw_queue_wait(Duration::from_secs(seconds));
    }
    config = with_runtime_and_usage(config, &args.production);
    if let Some(max) = args.admission.max_sessions {
        config = config.with_max_sessions(max);
    }
    if let Some(seconds) = args.admission.session_ttl_seconds {
        config = config.with_session_ttl(Duration::from_secs(seconds));
    }
    if !args.production.ephemeral_feedback {
        let path = args
            .production
            .feedback_file
            .or_else(|| std::env::var_os("FREELLAMA_FEEDBACK_FILE").map(PathBuf::from))
            .or_else(default_feedback_file)
            .context(
                "cannot determine a feedback path; pass --feedback-file or --ephemeral-feedback",
            )?;
        eprintln!(
            "freellama: persisting bounded placement feedback at {}",
            path.display()
        );
        config = config.with_feedback_file(path);
    }
    let token_file = args
        .production
        .auth_token_file
        .or_else(|| std::env::var_os("FREELLAMA_AUTH_TOKEN_FILE").map(PathBuf::from));
    if let Some(path) = token_file {
        let token = freellama::auth::read_token_file(&path)?;
        eprintln!(
            "freellama: bearer authentication enabled from {}",
            path.display()
        );
        config = config.with_auth_token(token);
    }
    if args.production.allow_remote {
        config = config.with_remote_access(true);
    }
    report_admission_config();
    serve_platform(config).await
}

pub(crate) fn configure_resource_policy(
    config: &mut PlatformConfig,
    args: &AdmissionArgs,
) -> Result<()> {
    if args.resource_hold_available_percent.is_some()
        || args.resource_resume_available_percent.is_some()
        || args.resource_telemetry_policy.is_some()
    {
        let mut policy = freellama::platform::resources::ResourcePolicy::default();
        if let Some(value) = args.resource_hold_available_percent {
            policy.hold_available_percent = value;
        }
        if let Some(value) = args.resource_resume_available_percent {
            policy.resume_available_percent = value;
        }
        if let Some(value) = args.resource_telemetry_policy {
            policy.telemetry_policy = value.into();
        }
        config.resource_governor = freellama::platform::resources::ResourceGovernor::new(policy)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// The live-tunable runtime file and the usage ledger (on by default, next to the feedback file).
fn with_runtime_and_usage(
    mut config: PlatformConfig,
    production: &ProductionArgs,
) -> PlatformConfig {
    if let Some(path) = &production.runtime_config {
        eprintln!("freellama: watching runtime config {}", path.display());
        config = config.with_runtime_config(path);
    }
    if !production.ephemeral_usage {
        let path = production
            .usage_file
            .clone()
            .or_else(|| std::env::var_os("FREELLAMA_USAGE_FILE").map(PathBuf::from))
            .or_else(|| default_feedback_file().map(|path| path.with_file_name("usage.jsonl")));
        if let Some(path) = path {
            eprintln!("freellama: recording task usage at {}", path.display());
            config = config.with_usage_file(path);
        }
    }
    config
}

fn report_admission_config() {
    eprintln!(
        "freellama: admission budgets are weighted units (embedding 1, chat 2, vision 4), not \
         literal task counts. Flags and env vars win over a --runtime-config file, and \
         `freellama config` shows each effective value and its source. Pair same-model GPU \
         concurrency changes with OLLAMA_NUM_PARALLEL and KV-cache validation."
    );
    eprintln!(
        "freellama: without an explicit GPU budget the default follows the Ollama server's \
         OLLAMA_NUM_PARALLEL (2 units per parallel slot)"
    );
}

fn default_feedback_file() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("FreeLlama").join("feedback.json"));
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|path| path.join("freellama").join("feedback.json"))
}

/// Look for a conventional config file next to the working directory, then at the repo root.
///
/// Only used when the corresponding flag is absent. Explicit flags always win; this exists so the
/// documented happy path does not require remembering two of them.
/// The working directory first (a project-local policy), then the per-user config directory, which
/// is what an MCP-autostarted `serve` — launched from whatever directory the client chose — can
/// rely on.
fn discover_config(name: &str) -> Option<PathBuf> {
    let local = PathBuf::from(name);
    if local.is_file() {
        return Some(local);
    }
    user_config_file(name)
}

fn user_config_file(name: &str) -> Option<PathBuf> {
    let dir = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(|path| PathBuf::from(path).join("FreeLlama"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|path| path.join("freellama"))
    }?;
    Some(dir.join(name)).filter(|path| path.is_file())
}
