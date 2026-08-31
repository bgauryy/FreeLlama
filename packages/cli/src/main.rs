use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use freellama::{
    RunConfig, Suite, compare, doctor,
    model_bench::{BenchConfig, benchmark_all},
    platform::{Objective, PlatformConfig, RouteInput, TaskKind, serve as serve_platform},
    proxy::{ProxyConfig, serve},
    run_suite, validate_endpoints, write_json,
};
use reqwest::Client;
use serde_json::{Value, json};

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
    /// Admission budget in cost units — embedding 1, chat 2, vision 4 (default 8, or
    /// `FREELLAMA_MAX_CONCURRENT_TASKS`). Match it to `OLLAMA_NUM_PARALLEL`: Ollama's default is 1,
    /// so a higher value bounds the burst rather than buying parallel decoding.
    #[arg(long)]
    max_concurrent_tasks: Option<usize>,
    /// Seconds a task may queue for admission before being refused with 503 (default 120, or
    /// `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`). Refusing fast matches Ollama's own `ErrMaxQueue`
    /// contract; waiting forever would hide load as unattributable latency.
    #[arg(long)]
    max_queue_wait_seconds: Option<u64>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the localhost model platform and preserve Ollama-compatible endpoints.
    Serve {
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        upstream: String,
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
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
        /// Refuse rather than return a route graded below this ("low" or "medium"). "medium"
        /// needs both a policy file and a benchmark report; without them every route grades "low"
        /// and this refuses — which is the point.
        #[arg(long)]
        min_confidence: Option<String>,
    },
    /// Recommend an installed route or a reviewed, side-effect-free model installation plan.
    Recommend {
        #[arg(long, default_value = "http://127.0.0.1:11435")]
        endpoint: String,
        #[arg(long, value_enum, default_value_t = TaskKind::Completion)]
        task: TaskKind,
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        /// Restrict installation planning to this exact model tag.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
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
        #[arg(long, value_enum, default_value_t = Objective::Balanced)]
        objective: Objective,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        context_tokens: Option<u64>,
        /// Attach an image (repeatable). Required for `--task vision`: without one the model is
        /// routed correctly but has nothing to look at, and says so.
        #[arg(long = "image")]
        images: Vec<PathBuf>,
        /// Read embedding input from a file, one item per line, instead of the positional prompt.
        /// Batching is far cheaper than one request per line.
        #[arg(long)]
        input_file: Option<PathBuf>,
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

#[tokio::main]
#[allow(clippy::too_many_lines)] // Keep the exhaustive CLI dispatch visible in one place.
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            listen,
            upstream,
            benchmark_report,
            policy_file,
            recommendation_catalog,
            intent_model,
            admission,
        } => {
            start_platform(
                listen,
                upstream,
                benchmark_report,
                policy_file,
                recommendation_catalog,
                intent_model,
                admission,
            )
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
            min_confidence,
        } => {
            request_route(
                endpoint,
                task,
                objective,
                model,
                session,
                context_tokens,
                min_confidence,
            )
            .await?;
        }
        Command::Recommend {
            endpoint,
            task,
            objective,
            model,
            context_tokens,
        } => request_recommendation(endpoint, task, objective, model, context_tokens).await?,
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
            images,
            input_file,
        } => {
            request_task(
                prompt,
                endpoint,
                task,
                objective,
                model,
                session,
                context_tokens,
                images,
                input_file,
            )
            .await?;
        }
        Command::Proxy {
            listen,
            upstream,
            allow_remote,
            request_timeout_seconds,
            auto_restart_ollama,
        } => {
            let config = ProxyConfig::new(listen, upstream, allow_remote)
                .with_request_timeout(std::time::Duration::from_secs(request_timeout_seconds))
                .with_auto_restart_ollama(auto_restart_ollama);
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

async fn start_platform(
    listen: String,
    upstream: String,
    benchmark_report: Option<PathBuf>,
    policy_file: Option<PathBuf>,
    recommendation_catalog: Option<PathBuf>,
    intent_model: String,
    admission: AdmissionArgs,
) -> Result<()> {
    // `minConfidence: "medium"` needs BOTH a policy file and a benchmark report, and requiring two
    // explicit flags meant almost nobody ever had them — the gate degraded to refusing
    // everything. Fall back to conventional paths so the common case works, and say which
    // files were picked up so the behaviour is never silent.
    let policy_file = policy_file.or_else(|| discover_config("platform.toml"));
    let benchmark_report = benchmark_report.or_else(|| discover_config("benchmark-report.json"));
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
        listen,
        upstream,
        benchmark_report,
        policy_file,
        intent_model,
    );
    if let Some(path) = recommendation_catalog {
        config = config.with_recommendation_catalog(path);
    }
    if let Some(slots) = admission.max_concurrent_tasks {
        config = config.with_max_concurrent_tasks(slots);
    }
    if let Some(seconds) = admission.max_queue_wait_seconds {
        config = config.with_max_queue_wait(Duration::from_secs(seconds));
    }
    eprintln!(
        "freellama: admission budget {} cost units (embedding 1, chat 2, vision 4). Raise with \
         --max-concurrent-tasks, and raise OLLAMA_NUM_PARALLEL with it — Ollama serializes at its \
         default of 1.",
        config.resolved_max_concurrent_tasks()
    );
    serve_platform(config).await
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

async fn request_route(
    endpoint: String,
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    session: Option<String>,
    context_tokens: Option<u64>,
    min_confidence: Option<String>,
) -> Result<()> {
    let mut route = route_input(task, objective, model, session, context_tokens);
    route.min_confidence = min_confidence;
    print_post(
        &endpoint,
        "/_freellama/v1/routes",
        &serde_json::to_value(route)?,
    )
    .await
}

async fn request_recommendation(
    endpoint: String,
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    context_tokens: Option<u64>,
) -> Result<()> {
    let route = route_input(task, objective, model, None, context_tokens);
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
    images: Vec<PathBuf>,
    input_file: Option<PathBuf>,
) -> Result<()> {
    let route = route_input(task, objective, model, session, context_tokens);
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

fn route_input(
    task: TaskKind,
    objective: Objective,
    model: Option<String>,
    session_id: Option<String>,
    context_tokens: Option<u64>,
) -> RouteInput {
    RouteInput {
        task,
        objective,
        model,
        session_id,
        context_tokens,
        ..RouteInput::default()
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
    let response = cli_client()
        .get(format!("{}{path}", endpoint.trim_end_matches('/')))
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
    let response = cli_client()
        .post(format!("{}{path}", endpoint.trim_end_matches('/')))
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(|e| explain_transport(endpoint, e))?;
    print_response(response).await
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
            "Ollama health, the 9 memory env vars, machine profile",
        ),
        (
            "models",
            "freellama models",
            "installed models, capabilities, residency",
        ),
        (
            "route",
            "freellama route --task <t>",
            "which model would be picked; no generation",
        ),
        (
            "run_task",
            "freellama task --task <t> --prompt <p>",
            "route AND execute one call",
        ),
        (
            "search_models",
            "(MCP only)",
            "browse ollama.com, inspect tags for memory fit",
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
    println!("\nCLI-only: serve, proxy, session, bench-all, eval, run, natural-route, recommend.");
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
