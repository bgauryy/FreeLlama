//! `freellama serve`: operator file discovery, platform configuration and startup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use freellama::platform::{PlatformConfig, serve as serve_platform};

use super::args::{AdmissionArgs, PlatformStartArgs, ProductionArgs};

/// Operator evidence files for `serve`: flag, then environment, then the working directory, then
/// the `FreeLlama` home.
///
/// `minConfidence: "medium"` needs BOTH a policy file and a benchmark report, and requiring two
/// explicit flags meant almost nobody ever had them — the gate degraded to refusing everything.
/// Fall back to conventional paths so the common case works (including an MCP-autostarted
/// `serve`, which has no flags), and say which files were picked up so it is never silent.
fn resolve_operator_files(
    policy_file: Option<PathBuf>,
    benchmark_report: Option<PathBuf>,
    home: Option<&Path>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let policy_file = policy_file.or_else(|| discover_config("platform.toml", home));
    let benchmark_report =
        benchmark_report.or_else(|| discover_config("benchmark-report.json", home));
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
             freellama: generate a policy with `freellama policy from-eval \
             --aggregate <harness aggregate.json> --task <task> --out platform.toml`"
        );
    }
    (policy_file, benchmark_report)
}

/// Refuse an unusable listen/auth/upstream combination before anything is printed or opened:
/// a refused start must not create the SQLite store or announce files it will never use.
fn validate_listener(
    listen: &str,
    upstream: &str,
    auth_token: Option<&str>,
    allow_remote: bool,
) -> Result<()> {
    let mut probe =
        PlatformConfig::new(listen, upstream, None, None).with_remote_access(allow_remote);
    if let Some(token) = auth_token {
        probe = probe.with_auth_token(token.to_owned());
    }
    probe.validate()
}

/// The bearer token file and its token, with the listener validated against them first.
fn authenticated_listener(args: &PlatformStartArgs) -> Result<Option<(PathBuf, String)>> {
    let token_file = args
        .production
        .auth_token_file
        .clone()
        .or_else(|| std::env::var_os("FREELLAMA_AUTH_TOKEN_FILE").map(PathBuf::from));
    let credentials = token_file
        .map(|path| freellama::auth::read_token_file(&path).map(|token| (path, token)))
        .transpose()?;
    validate_listener(
        &args.listen,
        &args.backends.upstream,
        credentials.as_ref().map(|(_, token)| token.as_str()),
        args.production.allow_remote,
    )?;
    Ok(credentials)
}

pub(crate) async fn start_platform(args: PlatformStartArgs) -> Result<()> {
    let credentials = authenticated_listener(&args)?;
    let home = args
        .production
        .home
        .clone()
        .or_else(freellama::store::default_home);
    let (policy_file, benchmark_report) =
        resolve_operator_files(args.policy_file, args.benchmark_report, home.as_deref());
    let mut config = PlatformConfig::new(
        args.listen,
        args.backends.upstream,
        benchmark_report,
        policy_file,
    );
    configure_resource_policy(&mut config, &args.admission)?;
    // Owned for the life of this function: dropping it stops the CPU Ollama when serve returns.
    let cpu_backend = if args.backends.spawn_cpu_backend {
        let log = home
            .as_deref()
            .filter(|_| !args.production.ephemeral)
            .map(|home| home.join("cpu-ollama.log"));
        Some(
            super::cpu_backend::spawn(
                args.backends.cpu_backend_port,
                &args.backends.ollama_binary,
                log.as_deref(),
            )
            .await?,
        )
    } else {
        None
    };
    let cpu_upstream = args
        .backends
        .cpu_upstream
        .or_else(|| cpu_backend.as_ref().map(|backend| backend.endpoint.clone()));
    if let Some(cpu_upstream) = cpu_upstream {
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
    config = with_runtime_and_store(config, &args.production, home.as_deref())?;
    config = with_keys_and_upstreams(config, &args.production, home.as_deref())?;
    if let Some(max) = args.admission.max_sessions {
        config = config.with_max_sessions(max);
    }
    if let Some(seconds) = args.admission.session_ttl_seconds {
        config = config.with_session_ttl(Duration::from_secs(seconds));
    }
    if let Some((path, token)) = credentials {
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
    let served = serve_platform(config).await;
    drop(cpu_backend);
    served
}

pub(crate) fn configure_resource_policy(
    config: &mut PlatformConfig,
    args: &AdmissionArgs,
) -> Result<()> {
    if args.resource_hold_available_percent.is_some()
        || args.resource_resume_available_percent.is_some()
        || args.resource_hold_load_per_cpu.is_some()
        || args.resource_resume_load_per_cpu.is_some()
        || args.resource_hold_gpu_busy_percent.is_some()
        || args.resource_resume_gpu_busy_percent.is_some()
        || args.resource_telemetry_policy.is_some()
        || args.low_memory.is_some()
    {
        let mut policy = freellama::platform::resources::ResourcePolicy::default();
        if let Some(value) = args.low_memory {
            policy.low_memory = value.into();
        }
        if let Some(value) = args.resource_hold_available_percent {
            policy.hold_available_percent = value;
        }
        if let Some(value) = args.resource_resume_available_percent {
            policy.resume_available_percent = value;
        }
        if let Some(value) = args.resource_hold_load_per_cpu {
            policy.hold_load_per_cpu = value;
        }
        if let Some(value) = args.resource_resume_load_per_cpu {
            policy.resume_load_per_cpu = value;
        }
        policy.gpu_busy = match (
            args.resource_hold_gpu_busy_percent,
            args.resource_resume_gpu_busy_percent,
        ) {
            (Some(hold_percent), Some(resume_percent)) => {
                Some(freellama::platform::resources::GpuBusyPolicy {
                    hold_percent,
                    resume_percent,
                })
            }
            (None, None) => None,
            _ => anyhow::bail!("both GPU busy thresholds are required"),
        };
        if let Some(value) = args.resource_telemetry_policy {
            policy.telemetry_policy = value.into();
        }
        config.resource_governor = freellama::platform::resources::ResourceGovernor::new(policy)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// The live-tunable runtime file and the shared database (on by default, in the home).
fn with_runtime_and_store(
    mut config: PlatformConfig,
    production: &ProductionArgs,
    home: Option<&Path>,
) -> Result<PlatformConfig> {
    let runtime = production
        .runtime_config
        .clone()
        .or_else(|| std::env::var_os("FREELLAMA_RUNTIME_CONFIG").map(PathBuf::from))
        .or_else(|| {
            home.map(|home| home.join("runtime.toml"))
                .filter(|path| path.is_file())
        });
    if let Some(path) = runtime {
        eprintln!("freellama: watching runtime config {}", path.display());
        config = config.with_runtime_config(path);
    }
    if production.ephemeral {
        return Ok(config);
    }
    let home = home.context("cannot determine the FreeLlama home; pass --home or --ephemeral")?;
    let path = home.join(freellama::store::DATABASE_FILE);
    let store = freellama::store::Store::open(&path)?;
    eprintln!(
        "freellama: keeping footprints, feedback, usage and the residency audit in {}",
        path.display()
    );
    Ok(config.with_store(store))
}

/// Client keys and OpenAI-compatible upstreams, from their flags or the home's `clients.toml`
/// and `upstreams.toml`.
fn with_keys_and_upstreams(
    mut config: PlatformConfig,
    production: &ProductionArgs,
    home: Option<&Path>,
) -> Result<PlatformConfig> {
    let in_home = |name: &str| {
        home.map(|home| home.join(name))
            .filter(|path| path.is_file())
    };
    if let Some(path) = production
        .client_keys
        .clone()
        .or_else(|| in_home("clients.toml"))
    {
        let keys = freellama::platform::load_client_keys(&path)?;
        eprintln!(
            "freellama: {} client key(s) from {}",
            keys.len(),
            path.display()
        );
        config = config.with_client_keys(keys);
    }
    if let Some(path) = production
        .upstreams
        .clone()
        .or_else(|| in_home("upstreams.toml"))
    {
        let upstreams = freellama::proxy::load_upstreams(&path)?;
        eprintln!(
            "freellama: {} OpenAI-compatible upstream(s) from {}: {}",
            upstreams.len(),
            path.display(),
            upstreams
                .iter()
                .map(|upstream| format!("{} at {}", upstream.name, upstream.base_url))
                .collect::<Vec<_>>()
                .join(", ")
        );
        config = config.with_upstreams(upstreams);
    }
    Ok(config)
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

/// Look for a conventional config file in the working directory (a project-local policy), then
/// in the `FreeLlama` home, which is what an MCP-autostarted `serve`, launched from whatever
/// directory the client chose, can rely on. Only used when the corresponding flag is absent.
fn discover_config(name: &str, home: Option<&Path>) -> Option<PathBuf> {
    let local = PathBuf::from(name);
    if local.is_file() {
        return Some(local);
    }
    home.map(|home| home.join(name))
        .filter(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::validate_listener;

    const UPSTREAM: &str = "http://127.0.0.1:11434";

    #[test]
    fn a_remote_listener_without_authentication_is_refused_up_front() {
        let error = validate_listener("0.0.0.0:11499", UPSTREAM, None, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-loopback"), "{error}");
        assert!(validate_listener("127.0.0.1:11499", UPSTREAM, None, false).is_ok());
        assert!(validate_listener("not-an-address", UPSTREAM, None, false).is_err());
        assert!(validate_listener("127.0.0.1:11499", UPSTREAM, None, true).is_err());
    }
}
