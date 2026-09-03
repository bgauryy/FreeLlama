use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use freellama::{
    RunConfig, Suite, compare, doctor,
    model_bench::{BenchConfig, Capability, benchmark_all},
    platform::{
        ExecutionPreference, Objective, PlacementEvidence, PlatformConfig, RouteInput, TaskKind,
        serve as serve_platform,
    },
    proxy::{ProxyConfig, serve},
    run_suite, validate_endpoints, write_json,
};
use reqwest::Client;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "freellama",
    version,
    about = "Run, route, and evaluate local models through Ollama"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Admission tuning, grouped so `start_platform` stays within a readable argument count.
#[derive(Debug, Clone, Copy, clap::Args)]
struct AdmissionArgs {
    /// Primary/GPU admission budget in weighted units — embedding 1, chat 2, vision 4 (default 2,
    /// or `FREELLAMA_MAX_CONCURRENT_TASKS`). This is not a literal task count.
    #[arg(long, alias = "gpu-admission-slots")]
    max_concurrent_tasks: Option<usize>,
    /// CPU-backend admission budget in weighted units (default 1, or
    /// `FREELLAMA_CPU_MAX_CONCURRENT_TASKS`).
    #[arg(long)]
    cpu_max_concurrent_tasks: Option<usize>,
    /// Seconds a task may queue for admission before being refused with 503 (default 120, or
    /// `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`). Refusing fast matches Ollama's own `ErrMaxQueue`
    /// contract; waiting forever would hide load as unattributable latency.
    #[arg(long)]
    max_queue_wait_seconds: Option<u64>,
    /// Bound raw Ollama-compatible proxy requests with immediate 503. This is a generic primary
    /// backend cap only; use managed tasks for weighted CPU/GPU admission.
    #[arg(long)]
    raw_proxy_max_concurrent_requests: Option<usize>,
    /// Maximum live session-affinity handles (default 1024). Sessions contain no prompt/KV data.
    #[arg(long)]
    max_sessions: Option<usize>,
    /// Expire idle affinity handles after this many seconds (default 3600).
    #[arg(long)]
    session_ttl_seconds: Option<u64>,
}

/// Ollama backend placement, grouped so device-specific routing stays an explicit serve concern.
#[derive(Debug, Clone, clap::Args)]
struct BackendArgs {
    #[arg(long, default_value = "http://127.0.0.1:11434")]
    upstream: String,
    /// Optional second loopback Ollama process forced to CPU. Managed tasks using a model
    /// named by --cpu-model are sent here; raw Ollama-compatible endpoints stay on --upstream.
    #[arg(long)]
    cpu_upstream: Option<String>,
    /// Model to assign to --cpu-upstream. Repeat for multiple models.
    #[arg(long, requires = "cpu_upstream")]
    cpu_model: Vec<String>,
}

/// Production state and network boundary for `serve`.
#[derive(Debug, Clone, clap::Args)]
struct ProductionArgs {
    /// Versioned adaptive-feedback snapshot. Defaults to the platform data directory.
    #[arg(long)]
    feedback_file: Option<PathBuf>,
    /// Disable feedback persistence explicitly (primarily for disposable tests).
    #[arg(long, conflicts_with = "feedback_file")]
    ephemeral_feedback: bool,
    /// File containing a bearer token of at least 32 bytes. The token is never accepted on the
    /// command line, where process inspection could expose it.
    #[arg(long)]
    auth_token_file: Option<PathBuf>,
    /// Permit a non-loopback listener. Requires --auth-token-file (or
    /// `FREELLAMA_AUTH_TOKEN_FILE`).
    #[arg(long)]
    allow_remote: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect prerequisites and print a side-effect-free first-run plan. Never pulls a model.
    Init {
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        ollama_endpoint: String,
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        serve_endpoint: String,
    },
    /// Generate a strong bearer token into a new mode-0600 file. Refuses to overwrite.
    AuthToken {
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the localhost model platform and preserve Ollama-compatible endpoints.
    Serve {
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
        #[command(flatten)]
        backends: BackendArgs,
        /// Optional machine-local benchmark report used for evidence-backed ranking.
        #[arg(long)]
        benchmark_report: Option<PathBuf>,
        /// Strict task policy containing evaluated candidate order.
        #[arg(long)]
        policy_file: Option<PathBuf>,
        /// Reviewed catalog used for side-effect-free model installation plans.
        #[arg(long)]
        recommendation_catalog: Option<PathBuf>,
        /// Small local Ollama model that translates natural language into route intent.
        #[arg(long, default_value = "qwen2.5:0.5b")]
        intent_model: String,
        #[command(flatten)]
        admission: AdmissionArgs,
        #[command(flatten)]
        production: ProductionArgs,
    },
    /// List installed local models with capabilities, residency, and local evidence.
    Models {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
    },
    /// Print the Mac execution profile visible to the platform.
    Machine {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
    },
    /// Create an isolated session for model affinity across related tasks.
    Session {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
    },
    /// Resolve a task to a local model and Ollama request profile without running it.
    Route {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
        #[arg(long, value_enum, default_value_t = TaskKind::Completion)]
        task: TaskKind,
        /// `balanced`/`quality` need a task policy; without one the router refuses. Use `fastest`
        /// until `freellama policy-from-eval` has produced one.
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
        /// Prefer an operator-configured backend. Falls back safely when it has no eligible model.
        #[arg(long, value_enum, default_value_t = ExecutionPreference::Auto)]
        execution_preference: ExecutionPreference,
        /// Require matching Ollama /api/ps proof instead of trusting only backend assignment.
        #[arg(long, value_enum, default_value_t = PlacementEvidence::Configured)]
        min_placement_evidence: PlacementEvidence,
        /// Refuse rather than return a route graded below this ("low" or "medium"). "medium"
        /// needs both a policy file and a benchmark report; without them every route grades "low"
        /// and this refuses — which is the point.
        #[arg(long)]
        min_confidence: Option<String>,
        /// Extra capability the model must advertise (repeatable), e.g. `--required-capability vision`.
        #[arg(long = "required-capability")]
        required_capabilities: Vec<String>,
    },
    /// Recommend an installed route or a reviewed, side-effect-free model installation plan.
    Recommend {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
        #[arg(long, value_enum, default_value_t = TaskKind::Completion)]
        task: TaskKind,
        /// `balanced`/`quality` need a task policy; without one the router refuses. Use `fastest`
        /// until `freellama policy-from-eval` has produced one.
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        /// Restrict installation planning to this exact model tag.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
        /// Prefer an operator-configured backend. Falls back safely when it has no eligible model.
        #[arg(long, value_enum, default_value_t = ExecutionPreference::Auto)]
        execution_preference: ExecutionPreference,
        #[arg(long, value_enum, default_value_t = PlacementEvidence::Configured)]
        min_placement_evidence: PlacementEvidence,
        #[arg(long = "required-capability")]
        required_capabilities: Vec<String>,
    },
    /// Translate natural language locally, then return the deterministic route.
    NaturalRoute {
        /// Natural-language task description.
        text: String,
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Resolve and execute one non-streaming task through the localhost platform.
    Task {
        /// Prompt to send as a single user message.
        prompt: String,
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
        #[arg(long, value_enum, default_value_t = TaskKind::Completion)]
        task: TaskKind,
        /// `balanced`/`quality` need a task policy; without one the router refuses. Use `fastest`
        /// until `freellama policy-from-eval` has produced one.
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
        /// Prefer an operator-configured backend. Falls back safely when it has no eligible model.
        #[arg(long, value_enum, default_value_t = ExecutionPreference::Auto)]
        execution_preference: ExecutionPreference,
        /// `observed` fails closed unless the selected resident model matches physical placement.
        #[arg(long, value_enum, default_value_t = PlacementEvidence::Configured)]
        min_placement_evidence: PlacementEvidence,
        /// Attach an image (repeatable). Required for `--task vision`: without one the model is
        /// routed correctly but has nothing to look at, and says so.
        #[arg(long = "image")]
        images: Vec<PathBuf>,
        /// Read embedding input from a file, one item per line, instead of the positional prompt.
        /// Batching is far cheaper than one request per line.
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Refuse rather than run a route graded below this ("low" or "medium").
        #[arg(long)]
        min_confidence: Option<String>,
        #[arg(long = "required-capability")]
        required_capabilities: Vec<String>,
    },
    /// Run an optional Ollama-compatible telemetry and policy sidecar.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        upstream: String,
        /// Explicitly permit binding beyond localhost. Add authentication before using this.
        #[arg(long)]
        allow_remote: bool,
        /// Per-attempt upstream timeout. Raise this for endpoints that legitimately run long
        /// (e.g. `/api/pull`); the default suits chat/generate-style requests.
        #[arg(long, default_value_t = 120)]
        request_timeout_seconds: u64,
        /// Opt-in: on a true connection-refused failure (Ollama's process is gone, not just
        /// slow or erroring), quit and relaunch the macOS Ollama app once, then retry the
        /// request. Off by default — this never happens unless explicitly enabled.
        #[arg(long)]
        auto_restart_ollama: bool,
        /// Optional immediate cap for raw compatibility requests. Prefer managed tasks for
        /// weighted admission and backend-aware routing.
        #[arg(long)]
        max_concurrent_requests: Option<usize>,
    },
    /// Benchmark every installed model in separate capability groups.
    BenchAll {
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        endpoint: String,
        /// Restrict the run to exact installed model names; repeat the flag as needed.
        #[arg(long)]
        include: Vec<String>,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 3)]
        trials: u32,
        #[arg(
            long,
            default_value = ".octocode/evals/evidence/latest-all-models.json"
        )]
        output: PathBuf,
    },
    /// Generate a routing policy from a quality benchmark aggregate.
    ///
    /// This is what makes `minConfidence: "medium"` reachable: the router only grades a route
    /// `medium` when the task has both a configured policy and benchmark data. Reads pass rates
    /// from a harness aggregate — NOT `bench-all`, which measures throughput, not correctness.
    PolicyFromEval {
        /// Path to a harness `aggregate.json`.
        #[arg(long)]
        aggregate: PathBuf,
        /// The task this suite actually measures. Named by you, never inferred.
        #[arg(long, value_enum)]
        task: TaskKind,
        /// Minimum `pass_at_1` to qualify.
        #[arg(long, default_value_t = 0.8)]
        min_pass: f64,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Accept a run with fewer than three trials, marking the output smoke-only.
        #[arg(long)]
        allow_smoke: bool,
        /// Endpoint used to list installed models (aggregate ids are lossy slugs).
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        ollama_endpoint: String,
    },
    /// List the MCP tool surface and the CLI command that does the same thing.
    ///
    /// For an agent that can run commands but cannot speak MCP: the same capability map the MCP
    /// server exposes, discoverable without a protocol client.
    Tools,
    /// Verify that an Ollama server exposes the required diagnostic APIs.
    Doctor {
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        endpoint: String,
    },
    /// Run the same frozen regression suite against isolated stock and candidate servers.
    Eval {
        #[arg(long)]
        baseline_url: String,
        #[arg(long)]
        candidate_url: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        secondary_model: Option<String>,
        #[arg(long, default_value = "benchmark/suites/ollama-mlx-regressions.json")]
        suite: PathBuf,
        #[arg(long, default_value = ".octocode/evals/latest-ollama-comparison.json")]
        output: PathBuf,
    },
    /// Run a frozen suite against one Ollama build to create a baseline or reproduction.
    Run {
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        endpoint: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        secondary_model: Option<String>,
        #[arg(long, default_value = "benchmark/suites/ollama-mlx-regressions.json")]
        suite: PathBuf,
        #[arg(long, default_value = ".octocode/evals/latest-ollama-run.json")]
        output: PathBuf,
    },
}

struct PlatformStartArgs {
    listen: String,
    backends: BackendArgs,
    benchmark_report: Option<PathBuf>,
    policy_file: Option<PathBuf>,
    recommendation_catalog: Option<PathBuf>,
    intent_model: String,
    admission: AdmissionArgs,
    production: ProductionArgs,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // Keep the exhaustive CLI dispatch visible in one place.
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            ollama_endpoint,
            serve_endpoint,
        } => initialize(ollama_endpoint, serve_endpoint).await?,
        Command::AuthToken { out } => generate_auth_token(&out)?,
        Command::Serve {
            listen,
            backends,
            benchmark_report,
            policy_file,
            recommendation_catalog,
            intent_model,
            admission,
            production,
        } => {
            start_platform(PlatformStartArgs {
                listen,
                backends,
                benchmark_report,
                policy_file,
                recommendation_catalog,
                intent_model,
                admission,
                production,
            })
            .await?;
        }
        Command::Models { endpoint } => {
            print_get(&endpoint, "/_freellama/v1/models").await?;
        }
        Command::Machine { endpoint } => {
            print_get(&endpoint, "/_freellama/v1/machine").await?;
        }
        Command::Session { endpoint } => {
            print_post(&endpoint, "/_freellama/v1/sessions", &json!({})).await?;
        }
        Command::Route {
            endpoint,
            task,
            objective,
            model,
            session,
            context_tokens,
            execution_preference,
            min_placement_evidence,
            min_confidence,
            required_capabilities,
        } => {
            request_route(
                endpoint,
                task,
                objective,
                model,
                session,
                context_tokens,
                execution_preference,
                min_placement_evidence,
                min_confidence,
                required_capabilities,
            )
            .await?;
        }
        Command::Recommend {
            endpoint,
            task,
            objective,
            model,
            context_tokens,
            execution_preference,
            min_placement_evidence,
            required_capabilities,
        } => {
            let route = route_input(
                task,
                objective,
                model,
                None,
                context_tokens,
                execution_preference,
                min_placement_evidence,
                &required_capabilities,
                None,
            );
            request_recommendation(endpoint, route).await?;
        }
        Command::NaturalRoute {
            text,
            endpoint,
            session,
        } => request_natural_route(text, endpoint, session).await?,
        Command::Task {
            prompt,
            endpoint,
            task,
            objective,
            model,
            session,
            context_tokens,
            execution_preference,
            min_placement_evidence,
            images,
            input_file,
            min_confidence,
            required_capabilities,
        } => {
            request_task(
                prompt,
                endpoint,
                task,
                objective,
                model,
                session,
                context_tokens,
                execution_preference,
                min_placement_evidence,
                images,
                input_file,
                min_confidence,
                required_capabilities,
            )
            .await?;
        }
        Command::Proxy {
            listen,
            upstream,
            allow_remote,
            request_timeout_seconds,
            auto_restart_ollama,
            max_concurrent_requests,
        } => {
            let mut config = ProxyConfig::new(listen, upstream, allow_remote)
                .with_request_timeout(std::time::Duration::from_secs(request_timeout_seconds))
                .with_auto_restart_ollama(auto_restart_ollama);
            if let Some(max) = max_concurrent_requests {
                config = config.with_max_concurrent_requests(max);
            }
            serve(config).await?;
        }
        Command::BenchAll {
            endpoint,
            include,
            timeout_seconds,
            trials,
            output,
        } => benchmark(endpoint, include, timeout_seconds, trials, output).await?,
        Command::PolicyFromEval {
            aggregate,
            task,
            min_pass,
            out,
            allow_smoke,
            ollama_endpoint,
        } => {
            let installed = installed_models(&ollama_endpoint).await?;
            let (qualified, benchmark_date) = freellama::policy::qualify_from_eval_path(
                &aggregate,
                &installed,
                min_pass,
                allow_smoke,
            )?;
            let smoke = qualified.iter().any(|q| q.trials < 3);
            let mut entries = std::collections::BTreeMap::new();
            entries.insert(task, qualified);
            let rendered = freellama::policy::render_policy(
                &entries,
                &aggregate.display().to_string(),
                &benchmark_date,
                min_pass,
                smoke,
            );
            match out {
                Some(path) => {
                    std::fs::write(&path, &rendered)?;
                    println!("wrote {}", path.display());
                }
                None => print!("{rendered}"),
            }
        }
        Command::Tools => print_tool_map(),
        Command::Doctor { endpoint } => print_doctor(&endpoint).await?,
        Command::Run {
            endpoint,
            model,
            secondary_model,
            suite,
            output,
        } => run_report(endpoint, model, secondary_model, suite, output).await?,
        Command::Eval {
            baseline_url,
            candidate_url,
            model,
            secondary_model,
            suite,
            output,
        } => {
            evaluate(
                baseline_url,
                candidate_url,
                model,
                secondary_model,
                suite,
                output,
            )
            .await?;
        }
    }
    Ok(())
}

async fn start_platform(args: PlatformStartArgs) -> Result<()> {
    // `minConfidence: "medium"` needs BOTH a policy file and a benchmark report, and requiring two
    // explicit flags meant almost nobody ever had them — the gate degraded to refusing
    // everything. Fall back to conventional paths so the common case works, and say which
    // files were picked up so the behaviour is never silent.
    let policy_file = args
        .policy_file
        .or_else(|| discover_config("platform.toml"));
    let benchmark_report = args
        .benchmark_report
        .or_else(|| discover_config("benchmark-report.json"));
    if let Some(p) = &policy_file {
        eprintln!("freellama: using policy file {}", p.display());
    }
    if let Some(b) = &benchmark_report {
        eprintln!("freellama: using benchmark report {}", b.display());
    }
    if policy_file.is_none() || benchmark_report.is_none() {
        eprintln!(
            "freellama: note — `minConfidence: \"medium\"` needs both a policy file and a \
             benchmark report; every route will grade `low` until both exist.\n\
             freellama: generate a policy with `freellama policy-from-eval --aggregate \
             <harness aggregate.json> --task <task> --out platform.toml`"
        );
    }

    let mut config = PlatformConfig::new(
        args.listen,
        args.backends.upstream,
        benchmark_report,
        policy_file,
        args.intent_model,
    );
    if let Some(cpu_upstream) = args.backends.cpu_upstream {
        eprintln!(
            "freellama: assigning {} model(s) to CPU Ollama at {cpu_upstream}",
            args.backends.cpu_model.len()
        );
        config = config.with_cpu_backend(cpu_upstream, args.backends.cpu_model);
    }
    if let Some(path) = args.recommendation_catalog {
        config = config.with_recommendation_catalog(path);
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
    if let Some(max) = args.admission.raw_proxy_max_concurrent_requests {
        config = config.with_raw_proxy_max_concurrent_requests(max);
    }
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
        let token = read_auth_token(&path)?;
        eprintln!(
            "freellama: bearer authentication enabled from {}",
            path.display()
        );
        config = config.with_auth_token(token);
    }
    if args.production.allow_remote {
        config = config.with_remote_access(true);
    }
    eprintln!(
        "freellama: per-backend admission budgets: GPU {} units, CPU {} units (embedding 1, chat \
         2, vision 4). These are weighted units, not literal task counts; pair same-model GPU \
         concurrency changes with OLLAMA_NUM_PARALLEL and KV-cache validation.",
        config.resolved_max_concurrent_tasks(),
        config.resolved_cpu_max_concurrent_tasks()
    );
    serve_platform(config).await
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

fn read_auth_token(path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect authentication token file {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "authentication token path must be a file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode().trailing_zeros() >= 6,
            "authentication token file {} must not be accessible by group or others (chmod 600)",
            path.display()
        );
    }
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("read authentication token file {}", path.display()))?;
    let token = token.trim().to_owned();
    ensure!(
        token.len() >= 32,
        "authentication token must be at least 32 bytes"
    );
    ensure!(
        !token.chars().any(char::is_whitespace),
        "authentication token must not contain whitespace"
    );
    Ok(token)
}

fn generate_auth_token(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create token directory {}", parent.display()))?;
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create new authentication token file {}", path.display()))?;
    writeln!(file, "{token}").context("write authentication token")?;
    file.sync_all().context("sync authentication token")?;
    println!("Created authentication token file {}", path.display());
    Ok(())
}

/// Side-effect-free first-run receipt. Initialization must discover the real host and inventory
/// before discussing a model, and discovery must never be interpreted as pull permission.
async fn initialize(ollama_endpoint: String, serve_endpoint: String) -> Result<()> {
    let diagnostics = match doctor(&ollama_endpoint).await {
        Ok(value) => json!({"status": "ok", "report": value}),
        Err(error) => json!({"status": "blocked", "error": error.to_string()}),
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build init HTTP client")?;
    let tags = match client
        .get(format!(
            "{}/api/tags",
            ollama_endpoint.trim_end_matches('/')
        ))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response
            .json::<Value>()
            .await
            .unwrap_or_else(|error| json!({"error": error.to_string()})),
        Ok(response) => json!({"error": format!("HTTP {}", response.status())}),
        Err(error) => json!({"error": error.to_string()}),
    };
    let health_request = client.get(format!(
        "{}/_freellama/v1/health",
        serve_endpoint.trim_end_matches('/')
    ));
    let health = match authenticate_request(health_request)?.send().await {
        Ok(response) if response.status().is_success() => {
            response.json::<Value>().await.unwrap_or_else(
                |error| json!({"status": "stale_or_invalid", "error": error.to_string()}),
            )
        }
        Ok(response) => json!({"status": "unavailable", "http_status": response.status().as_u16()}),
        Err(error) => json!({"status": "unavailable", "error": error.to_string()}),
    };
    let installed_models = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    let ollama_ready = diagnostics["status"] == "ok" && tags.get("error").is_none();
    let serve_ready = health["status"] == "ok";
    let mut next_steps = Vec::new();
    if !ollama_ready {
        next_steps.push(json!({
            "action": "install_or_start_ollama",
            "instruction": "Install Ollama from https://ollama.com/download, or start `ollama serve`, then rerun `freellama init`."
        }));
    } else if installed_models.is_empty() {
        next_steps.push(json!({
            "action": "choose_model",
            "instruction": "Describe the workload, modality, quality, context, download, disk, and memory constraints. Inspect exact tags and ask approval before `ollama pull`."
        }));
    }
    if ollama_ready && !serve_ready {
        next_steps.push(json!({
            "action": "start_freellama",
            "instruction": format!("Start `freellama serve --upstream {ollama_endpoint}`; add a second loopback Ollama and exact --cpu-model tags only after placement trials.")
        }));
    }
    if serve_ready && !installed_models.is_empty() {
        next_steps.push(json!({
            "action": "verify_managed_task",
            "instruction": "Preview with `freellama route --objective fastest`, run one bounded task with configured placement evidence, inspect execution.observation, then require observed evidence where placement matters."
        }));
        next_steps.push(json!({
            "action": "configure_mcp",
            "instruction": "Build packages/mcp, launch packages/mcp/dist/index.js over stdio, call doctor, then list installed/resident models before delegating."
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": if ollama_ready { "ready" } else { "blocked" },
            "ollama_endpoint": ollama_endpoint,
            "serve_endpoint": serve_endpoint,
            "diagnostics": diagnostics,
            "installed_models": installed_models,
            "serve_health": health,
            "next_steps": next_steps,
            "side_effects_performed": false,
            "model_pull_requires_exact_tag_approval": true
        }))?
    );
    Ok(())
}

async fn print_doctor(endpoint: &str) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&doctor(endpoint).await?)?
    );
    Ok(())
}

async fn request_natural_route(
    text: String,
    endpoint: String,
    session: Option<String>,
) -> Result<()> {
    let mut body = json!({"text": text});
    if let Some(session) = session {
        body["session_id"] = Value::String(session);
    }
    print_post(&endpoint, "/_freellama/v1/natural-routes", &body).await
}

#[allow(clippy::too_many_arguments)]
async fn request_route(
    endpoint: String,
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    session: Option<String>,
    context_tokens: Option<u64>,
    execution_preference: ExecutionPreference,
    min_placement_evidence: PlacementEvidence,
    min_confidence: Option<String>,
    required_capabilities: Vec<String>,
) -> Result<()> {
    let route = route_input(
        task,
        objective,
        model,
        session,
        context_tokens,
        execution_preference,
        min_placement_evidence,
        &required_capabilities,
        min_confidence,
    );
    print_post(
        &endpoint,
        "/_freellama/v1/routes",
        &serde_json::to_value(route)?,
    )
    .await
}

async fn request_recommendation(endpoint: String, route: RouteInput) -> Result<()> {
    print_post(
        &endpoint,
        "/_freellama/v1/recommendations",
        &serde_json::to_value(route)?,
    )
    .await
}

/// Nine parameters because `task` mirrors the platform's own request shape one-for-one; grouping
/// them into a struct here would add a type that exists only to satisfy a lint, and would drift
/// from the endpoint it mirrors.
#[allow(clippy::too_many_arguments)]
async fn request_task(
    prompt: String,
    endpoint: String,
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    session: Option<String>,
    context_tokens: Option<u64>,
    execution_preference: ExecutionPreference,
    min_placement_evidence: PlacementEvidence,
    images: Vec<PathBuf>,
    input_file: Option<PathBuf>,
    min_confidence: Option<String>,
    required_capabilities: Vec<String>,
) -> Result<()> {
    let route = route_input(
        task,
        objective,
        model,
        session,
        context_tokens,
        execution_preference,
        min_placement_evidence,
        &required_capabilities,
        min_confidence,
    );
    let mut body = serde_json::to_value(route)?;

    // Ollama takes images as base64 with no data-URI prefix, attached to the user message.
    // Without this the CLI could select a vision model but never hand it anything to look at —
    // `--task vision` was an advertised capability that could not actually be exercised.
    if !images.is_empty() {
        let encoded: Vec<String> = images
            .iter()
            .map(|path| {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("read image {}", path.display()))?;
                Ok(base64_encode(&bytes))
            })
            .collect::<Result<_>>()?;
        body["images"] = json!(encoded);
    }

    // Embedding input from a file, one item per line. Batching is dramatically cheaper than one
    // request per line, and this is what makes the CLI usable for indexing a corpus.
    if let Some(path) = &input_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read input file {}", path.display()))?;
        let items: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        ensure!(
            !items.is_empty(),
            "input file {} has no non-empty lines",
            path.display()
        );
        body["input"] = json!(items);
        return print_post(&endpoint, "/_freellama/v1/tasks", &body).await;
    }

    let field = if matches!(task, TaskKind::Embedding) {
        "input"
    } else {
        "prompt"
    };
    body[field] = Value::String(prompt);
    print_post(&endpoint, "/_freellama/v1/tasks", &body).await
}

#[allow(clippy::too_many_arguments)]
fn route_input(
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    session_id: Option<String>,
    context_tokens: Option<u64>,
    execution_preference: ExecutionPreference,
    min_placement_evidence: PlacementEvidence,
    required_capabilities: &[String],
    min_confidence: Option<String>,
) -> RouteInput {
    RouteInput {
        task,
        objective,
        model,
        session_id,
        context_tokens,
        execution_preference,
        min_placement_evidence,
        required_capabilities: required_capabilities
            .iter()
            .map(|s| Capability::parse(s))
            .collect(),
        min_confidence,
    }
}

async fn benchmark(
    endpoint: String,
    include: Vec<String>,
    timeout_seconds: u64,
    trials: u32,
    output: PathBuf,
) -> Result<()> {
    let report = benchmark_all(&BenchConfig {
        endpoint,
        include,
        timeout_seconds,
        trials,
    })
    .await?;
    write_json(&output, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_report(
    endpoint: String,
    model: String,
    secondary_model: Option<String>,
    suite_path: PathBuf,
    output: PathBuf,
) -> Result<()> {
    let suite = Suite::from_path(suite_path)?;
    let report = run_suite(
        &suite,
        &RunConfig {
            endpoint,
            primary_model: model,
            secondary_model,
        },
    )
    .await?;
    write_json(&output, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn evaluate(
    baseline_url: String,
    candidate_url: String,
    model: String,
    secondary_model: Option<String>,
    suite_path: PathBuf,
    output: PathBuf,
) -> Result<()> {
    validate_endpoints(&baseline_url, &candidate_url)?;
    let suite = Suite::from_path(suite_path)?;
    let baseline = run_suite(
        &suite,
        &RunConfig {
            endpoint: baseline_url,
            primary_model: model.clone(),
            secondary_model: secondary_model.clone(),
        },
    )
    .await?;
    let candidate = run_suite(
        &suite,
        &RunConfig {
            endpoint: candidate_url,
            primary_model: model,
            secondary_model,
        },
    )
    .await?;
    let comparison = compare(&baseline, &candidate, suite.defaults.target_improvement)?;
    write_json(&output, &comparison)?;
    println!("{}", serde_json::to_string_pretty(&comparison)?);
    if comparison.verdict == freellama::Verdict::Reject {
        std::process::exit(2);
    }
    Ok(())
}

/// One process-wide client for the CLI's control-plane calls.
///
/// `reqwest::Client` applies no request timeout unless asked, and builds a fresh connection pool
/// each time it is constructed. Both helpers below used `Client::new()` per call, so `freellama
/// route` (and every sibling subcommand) would hang indefinitely against a server that accepted
/// the connection and then never answered. The NAPI layer had the same defect and was fixed the
/// same way; this brings the CLI in line.
fn cli_client() -> Client {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(Client::new).clone()
}

/// Decision-only control-plane calls: computation over an in-memory model list, so seconds means
/// wedged, not busy. Overridable via `FREELLAMA_CONTROL_TIMEOUT_SECONDS` (same name the NAPI layer
/// reads, so one setting covers CLI and MCP alike).
fn cli_control_timeout() -> Duration {
    freellama::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        freellama::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}

/// Calls that make a model actually generate (`/tasks`, `/natural-routes`). A cold load of a large
/// model can take minutes, so this has to be generous or it aborts work that would have succeeded.
fn cli_task_timeout() -> Duration {
    freellama::timeout_from_env(
        "FREELLAMA_TASK_TIMEOUT_SECONDS",
        freellama::DEFAULT_TASK_TIMEOUT_SECS,
    )
}

/// Turn a transport failure into an actionable message.
///
/// Every control-plane subcommand needs a running `freellama serve`, and the raw reqwest error
/// ("client error (Connect)") does not say so. Naming the missing process and the command that
/// starts it is the difference between a dead end and a next step.
fn explain_transport(endpoint: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_connect() || error.is_timeout() {
        return anyhow::anyhow!(
            "cannot reach the FreeLlama control plane at {endpoint}.\n\
             Start it with:\n  freellama serve --recommendation-catalog recommendations.example.toml\n\
             (`doctor` is the one subcommand that works without it.)\n\
             Underlying error: {error}"
        );
    }
    error.into()
}

/// Print a JSON response, or fail with the server's own explanation of why it refused.
///
/// `error_for_status()` discards the body, which is where every useful refusal lives — a
/// `min_confidence` refusal names the grade, the evidence, the model it would have picked and the
/// two commands that raise the grade, and all of that was being replaced by a bare
/// "422 Unprocessable Entity". Surface the server's own explanation instead.
///
/// A success whose body is not JSON is an error, not silence: printing nothing and exiting 0 would
/// tell the caller the command worked when nothing was reported.
async fn print_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = response.text().await.context("read response body")?;
    let payload = serde_json::from_str::<Value>(&body).ok();
    if !status.is_success() {
        let detail = payload
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
            .map_or_else(
                || {
                    if body.trim().is_empty() {
                        status.to_string()
                    } else {
                        body.trim().to_owned()
                    }
                },
                ToOwned::to_owned,
            );
        anyhow::bail!("{detail}");
    }
    let value = payload.with_context(|| {
        format!(
            "{status} response from the platform was not JSON: {}",
            body.trim()
        )
    })?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn print_get(endpoint: &str, path: &str) -> Result<()> {
    let request = cli_client().get(format!("{}{path}", endpoint.trim_end_matches('/')));
    let response = authenticate_request(request)?
        .timeout(cli_control_timeout())
        .send()
        .await
        .map_err(|e| explain_transport(endpoint, e))?;
    print_response(response).await
}

async fn print_post(endpoint: &str, path: &str, body: &Value) -> Result<()> {
    // `/tasks` and `/natural-routes` run a model; everything else is a decision.
    let timeout = if path.ends_with("/tasks") || path.ends_with("/natural-routes") {
        cli_task_timeout()
    } else {
        cli_control_timeout()
    };
    let request = cli_client()
        .post(format!("{}{path}", endpoint.trim_end_matches('/')))
        .timeout(timeout)
        .json(body);
    let response = authenticate_request(request)?
        .send()
        .await
        .map_err(|e| explain_transport(endpoint, e))?;
    print_response(response).await
}

fn authenticate_request(request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
    let Some(path) = std::env::var_os("FREELLAMA_AUTH_TOKEN_FILE").map(PathBuf::from) else {
        return Ok(request);
    };
    Ok(request.bearer_auth(read_auth_token(&path)?))
}

/// Print the MCP tool surface alongside its CLI equivalent.
///
/// Hand-maintained rather than generated from the MCP server, so the CLI keeps no Node dependency.
/// The trade-off is that adding or removing a tool means updating this table.
fn print_tool_map() {
    println!("FreeLlama exposes 8 MCP tools. Equivalents for a CLI-only agent:\n");
    let rows = [
        (
            "doctor",
            "freellama doctor",
            "Ollama health, 11 memory settings, machine profile",
        ),
        (
            "models",
            "freellama models",
            "installed estate; view library (ollama.com) is MCP-only",
        ),
        (
            "run_task",
            "freellama task --task <t> <prompt>",
            "route AND execute; preview:true is decision-only",
        ),
        (
            "run_task_batch",
            "HTTP/MCP only",
            "bounded, independent-only fair local dispatch",
        ),
        (
            "session",
            "freellama session",
            "create/release bounded model-affinity metadata; no prompt/KV",
        ),
        (
            "ollama_manage",
            "ollama pull <m> / ollama stop <m>",
            "download or unload a model",
        ),
        (
            "ollama_delete",
            "ollama rm <m>",
            "DESTRUCTIVE. Only when a human names the model",
        ),
        (
            "delegate_research",
            "(MCP only)",
            "grounded answer from a local model reading files",
        ),
    ];
    for (tool, cli, what) in rows {
        println!("  {tool:<18} {cli:<38} {what}");
    }
    println!(
        "\nCLI-only: init, serve, proxy, machine, session, bench-all, policy-from-eval, eval, run, \
         natural-route, recommend."
    );
    println!("Orchestration guidance for either surface: skills/freellama/SKILL.md");
}

/// List installed model tags. Needed because a harness aggregate identifies models by a lossy
/// slug (`:` replaced with `-`), which cannot be reversed unambiguously — matching forward from
/// real tags is the only reliable direction.
async fn installed_models(endpoint: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Tags {
        models: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
    }
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let tags: Tags = cli_client()
        .get(&url)
        .timeout(cli_control_timeout())
        .send()
        .await
        .with_context(|| format!("list installed models from {url}"))?
        .error_for_status()?
        .json()
        .await?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
}

/// Look for a conventional config file next to the working directory, then at the repo root.
///
/// Only used when the corresponding flag is absent. Explicit flags always win; this exists so the
/// documented happy path does not require remembering two of them.
fn discover_config(name: &str) -> Option<PathBuf> {
    for dir in [".", ".."] {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Minimal base64 encoder — avoids pulling a dependency into the CLI for one call site.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
