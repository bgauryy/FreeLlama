//! Command-line arguments: the subcommand tree and its shared flag groups.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use freellama::platform::{
    ExecutionPreference, Objective, PlacementEvidence, TaskKind, TaskPriority,
};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "freellama",
    version,
    about = "Run and route local models through Ollama"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Admission tuning, grouped so `start_platform` stays within a readable argument count.
#[derive(Debug, Clone, Copy, clap::Args)]
pub(crate) struct AdmissionArgs {
    /// Required local telemetry: RAM by default; best-effort explicitly permits unknown RAM.
    #[arg(long, value_enum)]
    pub(crate) resource_telemetry_policy: Option<TelemetryPolicyArg>,
    /// Available host-memory percentage held back before admitting model work (default 15).
    #[arg(long)]
    pub(crate) resource_hold_available_percent: Option<u32>,
    /// Available-memory percentage required for recovery (default 20; greater than hold).
    #[arg(long)]
    pub(crate) resource_resume_available_percent: Option<u32>,
    /// Primary/GPU admission budget in weighted units — embedding 1, chat 2, vision 4 (default 2
    /// per Ollama parallel slot, or the runtime file). This is not a literal task count.
    #[arg(long, alias = "gpu-admission-slots")]
    pub(crate) max_concurrent_tasks: Option<usize>,
    /// CPU-backend admission budget in weighted units (default 1, or the
    /// runtime file).
    #[arg(long)]
    pub(crate) cpu_max_concurrent_tasks: Option<usize>,
    /// Seconds a task may queue for admission before being refused with 503 (default 120, or
    /// `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`). Refusing fast matches Ollama's own `ErrMaxQueue`
    /// contract; waiting forever would hide load as unattributable latency.
    #[arg(long)]
    pub(crate) max_queue_wait_seconds: Option<u64>,
    /// Maximum managed requests retained while the primary/GPU pool is saturated (default 16,
    /// or the runtime file). Excess work receives 503 immediately.
    #[arg(long)]
    pub(crate) max_queued_tasks: Option<usize>,
    /// Maximum managed requests retained while the CPU pool is saturated (default 8, or the
    /// runtime file).
    #[arg(long)]
    pub(crate) cpu_max_queued_tasks: Option<usize>,
    /// Bound raw Ollama-compatible proxy streams (default 1 in `serve`). Over the cap a request
    /// waits up to --raw-queue-wait-seconds, then gets 429 with Retry-After. This is a generic
    /// primary-backend cap; use managed tasks for weighted CPU/GPU admission.
    #[arg(long)]
    pub(crate) raw_proxy_max_concurrent_requests: Option<usize>,
    /// How long a raw generation may wait for a slot or for managed execution (default 10, or the
    /// runtime file; 0 refuses at once).
    #[arg(long)]
    pub(crate) raw_queue_wait_seconds: Option<u64>,
    /// Maximum live session-affinity handles (default 1024). Sessions contain no prompt/KV data.
    #[arg(long)]
    pub(crate) max_sessions: Option<usize>,
    /// Expire idle affinity handles after this many seconds (default 3600).
    #[arg(long)]
    pub(crate) session_ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum TelemetryPolicyArg {
    BestEffort,
    RequireMemory,
    RequireAll,
}

impl From<TelemetryPolicyArg> for freellama::platform::resources::TelemetryPolicy {
    fn from(value: TelemetryPolicyArg) -> Self {
        match value {
            TelemetryPolicyArg::BestEffort => Self::BestEffort,
            TelemetryPolicyArg::RequireMemory => Self::RequireMemory,
            TelemetryPolicyArg::RequireAll => Self::RequireAll,
        }
    }
}

#[cfg(test)]
mod telemetry_cli_tests {
    use freellama::platform::PlatformConfig;

    use super::*;
    use crate::init::initialization_status;
    use crate::serve_cmd::configure_resource_policy;

    #[test]
    fn initialization_is_ready_only_with_runtime_service_and_installed_models() {
        assert_eq!(initialization_status(false, true, true), "blocked");
        assert_eq!(initialization_status(true, false, true), "setup_required");
        assert_eq!(initialization_status(true, true, false), "setup_required");
        assert_eq!(initialization_status(true, true, true), "ready");
    }

    #[test]
    fn job_removal_requires_one_valid_id_and_cannot_also_cancel() {
        let id = "b0bf3d83-4e2e-4bcb-a11e-a3c7a8a66d54";
        let parsed = Cli::try_parse_from(["freellama", "jobs", "--id", id, "--remove"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Jobs {
                id: Some(_),
                remove: true,
                cancel: false,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["freellama", "jobs", "--remove"]).is_err());
        assert!(
            Cli::try_parse_from(["freellama", "jobs", "--id", "../other", "--remove"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["freellama", "jobs", "--id", id, "--cancel", "--remove"]).is_err()
        );
    }

    #[test]
    fn telemetry_policy_parses_on_serve_and_proxy_and_rejects_typos() {
        for command in ["serve", "proxy"] {
            for policy in ["best-effort", "require-memory", "require-all"] {
                Cli::try_parse_from(["freellama", command, "--resource-telemetry-policy", policy])
                    .expect("supported telemetry policy");
            }
            assert!(
                Cli::try_parse_from([
                    "freellama",
                    command,
                    "--resource-telemetry-policy",
                    "allow-all"
                ])
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn explicit_policy_reaches_the_governor_and_default_requires_memory() {
        for (value, expected) in [
            ("best-effort", "best_effort"),
            ("require-memory", "require_memory"),
            ("require-all", "require_all"),
        ] {
            let parsed =
                Cli::try_parse_from(["freellama", "serve", "--resource-telemetry-policy", value])
                    .unwrap();
            let Command::Serve {
                admission: args, ..
            } = parsed.command
            else {
                panic!("expected serve")
            };
            let mut config = PlatformConfig::new(
                "127.0.0.1:11435",
                freellama::DEFAULT_OLLAMA_ENDPOINT,
                None,
                None,
            );
            configure_resource_policy(&mut config, &args).unwrap();
            assert_eq!(
                serde_json::to_value(
                    config
                        .resource_governor
                        .snapshot()
                        .await
                        .policy
                        .telemetry_policy
                )
                .unwrap(),
                expected
            );
        }
        // Memory telemetry is required where a collector exists; Windows has none, so its
        // default is best-effort rather than refusing every local request.
        let default_policy = if cfg!(any(target_os = "linux", target_os = "macos")) {
            "require_memory"
        } else {
            "best_effort"
        };
        assert_eq!(
            serde_json::to_value(
                freellama::platform::resources::ResourcePolicy::default().telemetry_policy
            )
            .unwrap(),
            default_policy
        );
    }
}

/// Ollama backend placement, grouped so device-specific routing stays an explicit serve concern.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct BackendArgs {
    #[arg(
        long,
        env = "FREELLAMA_OLLAMA_ENDPOINT",
        default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
    )]
    pub(crate) upstream: String,
    /// Optional second loopback Ollama process forced to CPU. Managed tasks using a model
    /// named by --cpu-model are sent here; raw Ollama-compatible endpoints stay on --upstream.
    #[arg(long)]
    pub(crate) cpu_upstream: Option<String>,
    /// Model to assign to --cpu-upstream. Repeat for multiple models.
    #[arg(long, requires = "cpu_upstream")]
    pub(crate) cpu_model: Vec<String>,
}

/// Per-task execution controls, separate from side-effect-free routing.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct TaskExecutionArgs {
    /// Override model residency; omitted uses the server adaptive finite TTL.
    #[arg(long)]
    pub(crate) keep_alive: Option<String>,
    /// Return a job ID immediately; use `jobs` to inspect or cancel it.
    #[arg(long)]
    pub(crate) defer: bool,
    /// Total deadline including discovery, waiting, loading, and inference; capped by server.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) timeout_seconds: Option<u64>,
    /// Fair admission class; interactive tasks receive more turns without starving background work.
    #[arg(long, value_enum, default_value_t = TaskPriority::Normal)]
    pub(crate) priority: TaskPriority,
    /// Admission/resource wait budget; capped by server.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) max_wait_seconds: Option<u64>,
}

/// Production state and network boundary for `serve`.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct ProductionArgs {
    /// Versioned adaptive-feedback snapshot. Defaults to the platform data directory.
    #[arg(long)]
    pub(crate) feedback_file: Option<PathBuf>,
    /// Disable feedback persistence explicitly (primarily for disposable tests).
    #[arg(long, conflicts_with = "feedback_file")]
    pub(crate) ephemeral_feedback: bool,
    /// File containing a bearer token of at least 32 bytes. The token is never accepted on the
    /// command line, where process inspection could expose it.
    #[arg(long)]
    pub(crate) auth_token_file: Option<PathBuf>,
    /// Permit a non-loopback listener. Requires --auth-token-file (or
    /// `FREELLAMA_AUTH_TOKEN_FILE`).
    #[arg(long)]
    pub(crate) allow_remote: bool,
    /// Live-tunable settings (TOML), re-read when the file changes. Defaults to
    /// `FREELLAMA_RUNTIME_CONFIG`; see `freellama.runtime.example.toml`.
    #[arg(long)]
    pub(crate) runtime_config: Option<PathBuf>,
    /// JSON-lines usage ledger, replayed at startup for daily totals. Defaults to the platform
    /// data directory (or `FREELLAMA_USAGE_FILE`).
    #[arg(long)]
    pub(crate) usage_file: Option<PathBuf>,
    /// Keep usage totals in memory only.
    #[arg(long, conflicts_with = "usage_file")]
    pub(crate) ephemeral_usage: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Inspect prerequisites and print a side-effect-free first-run plan. Never pulls a model.
    Init {
        #[arg(
            long,
            env = "FREELLAMA_OLLAMA_ENDPOINT",
            default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
        )]
        ollama_endpoint: String,
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
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
        /// Optional machine-local benchmark report used for evidence-backed ranking. Defaults to
        /// `benchmark-report.json` in the working directory, then the user config directory.
        #[arg(long, env = "FREELLAMA_BENCHMARK_REPORT")]
        benchmark_report: Option<PathBuf>,
        /// Strict task policy containing evaluated candidate order. Defaults to `platform.toml` in
        /// the working directory, then the user config directory (`~/.config/freellama/`).
        #[arg(long, env = "FREELLAMA_POLICY_FILE")]
        policy_file: Option<PathBuf>,
        #[command(flatten)]
        admission: AdmissionArgs,
        #[command(flatten)]
        production: ProductionArgs,
    },
    /// List installed local models with capabilities, residency, and local evidence.
    Models {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
    },
    /// Print the machine execution profile (CPU, memory, GPU, backends) visible to the platform.
    Machine {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
    },
    /// Live view: admission and queues per backend, loaded models, host memory and GPU, circuit
    /// breakers, adaptive limits, Ollama's effective settings, and today's usage.
    Status {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
    },
    /// Usage totals per day and per model (tasks, errors, tokens, busy and queue time).
    Usage {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
        #[arg(long, default_value_t = 7)]
        days: u32,
    },
    /// Effective runtime settings and where each came from; --reload re-reads the config file.
    Config {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
        #[arg(long)]
        reload: bool,
    },
    /// Create an isolated session for model affinity across related tasks.
    Session {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
    },
    /// Manage opt-in process-local conversation history.
    Scope {
        #[arg(
            long,
            global = true,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
        #[command(subcommand)]
        action: crate::context_cli::ScopeAction,
    },
    /// Preload an exact installed chat model through managed resource admission.
    Warm(crate::context_cli::WarmArgs),
    /// Resolve a task to a local model and Ollama request profile without running it.
    Route(RouteArgs),
    /// Resolve and execute one non-streaming task through the localhost platform.
    Task(TaskArgs),
    /// List deferred tasks, read one result, or cancel/remove one task by ID.
    Jobs {
        #[arg(
            long,
            env = "FREELLAMA_SERVE_ENDPOINT",
            default_value = freellama::DEFAULT_SERVE_ENDPOINT
        )]
        endpoint: String,
        #[arg(long)]
        id: Option<Uuid>,
        #[arg(long, requires = "id", conflicts_with = "remove")]
        cancel: bool,
        /// Stop active work, release local permits, then remove its retained record.
        #[arg(long, requires = "id")]
        remove: bool,
    },
    /// Run an optional Ollama-compatible telemetry and policy sidecar.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:11435")]
        listen: String,
        #[arg(
            long,
            env = "FREELLAMA_OLLAMA_ENDPOINT",
            default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
        )]
        upstream: String,
        /// Required local telemetry; defaults to require-memory like managed execution.
        #[arg(long, value_enum)]
        resource_telemetry_policy: Option<TelemetryPolicyArg>,
        /// Explicitly permit binding beyond localhost. Add authentication before using this.
        #[arg(long)]
        allow_remote: bool,
        /// Longest silence allowed from Ollama, including the wait for headers while a model
        /// loads. Not a total deadline: a stream that keeps producing bytes is never cut off.
        /// The default matches Ollama's own 5-minute model load timeout.
        #[arg(long, default_value_t = 300)]
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
        #[arg(
            long,
            env = "FREELLAMA_OLLAMA_ENDPOINT",
            default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
        )]
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
    /// Verify that an Ollama server exposes the required diagnostic APIs.
    Doctor {
        #[arg(
            long,
            env = "FREELLAMA_OLLAMA_ENDPOINT",
            default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
        )]
        endpoint: String,
    },
    /// Generate routing policy files for `serve --policy-file`.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum PolicyAction {
    /// Write a task policy from a harness quality aggregate (`pass_at_1`), never from throughput.
    ///
    /// Aggregate model ids are lossy slugs (`:` replaced with `-`), so qualification matches
    /// forward from the tags installed on the Ollama endpoint (`/api/tags`).
    FromEval(PolicyFromEvalArgs),
}

#[derive(Debug, clap::Args)]
pub(crate) struct PolicyFromEvalArgs {
    /// Harness aggregate.json.
    #[arg(long)]
    pub(crate) aggregate: PathBuf,
    /// The task this suite measures; never inferred. Accepts `-` or `_` (`code-repair`).
    #[arg(long, value_parser = freellama::platform::policy_eval::parse_task_kind)]
    pub(crate) task: TaskKind,
    /// Minimum `pass_at_1` to qualify.
    #[arg(long, default_value_t = 0.8)]
    pub(crate) min_pass: f64,
    /// Write here instead of stdout.
    #[arg(long)]
    pub(crate) out: Option<PathBuf>,
    /// Accept fewer than three trials; marks the output smoke-only, which `serve` refuses.
    #[arg(long)]
    pub(crate) allow_smoke: bool,
    /// Endpoint used to list installed models (aggregate ids are lossy slugs).
    #[arg(
        long,
        env = "FREELLAMA_OLLAMA_ENDPOINT",
        default_value = freellama::DEFAULT_OLLAMA_ENDPOINT
    )]
    pub(crate) ollama_endpoint: String,
}

pub(crate) struct PlatformStartArgs {
    pub(crate) listen: String,
    pub(crate) backends: BackendArgs,
    pub(crate) benchmark_report: Option<PathBuf>,
    pub(crate) policy_file: Option<PathBuf>,
    pub(crate) admission: AdmissionArgs,
    pub(crate) production: ProductionArgs,
}

// Flags of the `route` subcommand (help text comes from the variant's doc comment).
#[derive(Debug, clap::Args)]
pub(crate) struct RouteArgs {
    #[arg(
        long,
        env = "FREELLAMA_SERVE_ENDPOINT",
        default_value = freellama::DEFAULT_SERVE_ENDPOINT
    )]
    pub(crate) endpoint: String,
    #[arg(long, value_enum, default_value_t = TaskKind::Completion)]
    pub(crate) task: TaskKind,
    /// `balanced` prefers task-policy candidates, falling back with low confidence.
    /// `quality` needs a policy unless an explicit model is supplied.
    #[arg(long, value_enum, default_value_t = Objective::Balanced)]
    pub(crate) objective: Objective,
    #[arg(long)]
    pub(crate) model: Option<String>,
    #[arg(long)]
    pub(crate) session: Option<String>,
    #[arg(long)]
    pub(crate) context_tokens: Option<u64>,
    /// Prefer an operator-configured backend. Falls back safely when it has no eligible model.
    #[arg(long, value_enum, default_value_t = ExecutionPreference::Auto)]
    pub(crate) execution_preference: ExecutionPreference,
    /// Require matching Ollama /api/ps proof instead of trusting only backend assignment.
    #[arg(long, value_enum, default_value_t = PlacementEvidence::Configured)]
    pub(crate) min_placement_evidence: PlacementEvidence,
    /// Refuse rather than return a route graded below this ("low" or "medium"). "medium"
    /// needs both a policy file and a benchmark report; without them every route grades "low"
    /// and this refuses — which is the point.
    #[arg(long, value_parser = ["low", "medium"])]
    pub(crate) min_confidence: Option<String>,
    /// Extra capability the model must advertise (repeatable), e.g. `--required-capability vision`.
    #[arg(long = "required-capability")]
    pub(crate) required_capabilities: Vec<String>,
}

// Flags of the `task` subcommand (help text comes from the variant's doc comment).
#[derive(Debug, clap::Args)]
pub(crate) struct TaskArgs {
    /// Prompt to send as a single user message.
    pub(crate) prompt: String,
    #[arg(
        long,
        env = "FREELLAMA_SERVE_ENDPOINT",
        default_value = freellama::DEFAULT_SERVE_ENDPOINT
    )]
    pub(crate) endpoint: String,
    #[arg(long, value_enum)]
    pub(crate) task: Option<TaskKind>,
    /// `balanced` prefers task-policy candidates, falling back with low confidence.
    /// `quality` needs a policy unless an explicit model is supplied.
    #[arg(long, value_enum)]
    pub(crate) objective: Option<Objective>,
    #[arg(long)]
    pub(crate) model: Option<String>,
    #[arg(long)]
    pub(crate) session: Option<String>,
    #[arg(long)]
    pub(crate) context_tokens: Option<u64>,
    #[arg(long, requires = "scope_revision")]
    pub(crate) scope_id: Option<Uuid>,
    #[arg(long, requires = "scope_id")]
    pub(crate) scope_revision: Option<u64>,
    /// Prefer an operator-configured backend. Falls back safely when it has no eligible model.
    #[arg(long, value_enum)]
    pub(crate) execution_preference: Option<ExecutionPreference>,
    /// `observed` fails closed unless the selected resident model matches physical placement.
    #[arg(long, value_enum)]
    pub(crate) min_placement_evidence: Option<PlacementEvidence>,
    /// Attach an image (repeatable). Required for `--task vision`: without one the model is
    /// routed correctly but has nothing to look at, and says so.
    #[arg(long = "image")]
    pub(crate) images: Vec<PathBuf>,
    /// Read embedding input from a file, one item per line, instead of the positional prompt.
    /// Batching is far cheaper than one request per line.
    #[arg(long)]
    pub(crate) input_file: Option<PathBuf>,
    /// Refuse rather than run a route graded below this ("low" or "medium").
    #[arg(long, value_parser = ["low", "medium"])]
    pub(crate) min_confidence: Option<String>,
    #[arg(long = "required-capability")]
    pub(crate) required_capabilities: Vec<String>,
    #[command(flatten)]
    pub(crate) execution: TaskExecutionArgs,
}
