//! Platform server configuration: construction, validation and resolved timeouts.

use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::Url;

use super::{resources, runtime};
use crate::proxy::ProxyConfig;

#[derive(Clone)]
pub struct PlatformConfig {
    /// Shared host pressure policy; clones coordinate CPU/GPU/raw reservations.
    pub resource_governor: resources::ResourceGovernor,
    pub listen: String,
    /// Primary Ollama instance. The byte-preserving fallback proxy always targets this backend.
    pub upstream: String,
    /// Optional second loopback Ollama instance forced to CPU by its own process configuration.
    pub cpu_upstream: Option<String>,
    /// Models whose discovery and managed execution belong to `cpu_upstream`.
    pub cpu_models: BTreeSet<String>,
    pub benchmark_report: Option<PathBuf>,
    pub policy_file: Option<PathBuf>,
    /// Concurrent managed tasks allowed against Ollama, in cost units. `None` falls back to
    /// the runtime file, then to two units per Ollama parallel slot. This is the primary/GPU backend's weighted
    /// admission budget; the legacy field name is retained for compatibility.
    pub max_concurrent_tasks: Option<usize>,
    /// Weighted admission budget for the optional CPU backend. `None` falls back to
    /// the runtime file, then to 1.
    pub cpu_max_concurrent_tasks: Option<usize>,
    /// Longest a task may wait for admission and a transition lock together before 503. `None` falls
    /// back to `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, the runtime file, then 120s.
    pub max_queue_wait: Option<Duration>,
    /// Maximum managed requests waiting for the primary/GPU pool. Active requests are counted
    /// separately. `None` falls back to the runtime file, then to 16.
    pub max_queued_tasks: Option<usize>,
    /// Maximum managed requests waiting for the optional CPU pool. `None` falls back to
    /// the runtime file, then to 8.
    pub cpu_max_queued_tasks: Option<usize>,
    /// Maximum live affinity handles. Sessions hold metadata only, never prompt text or Ollama KV.
    pub max_sessions: Option<usize>,
    /// Idle lifetime for affinity handles. Successful route/task use refreshes the TTL.
    pub session_ttl: Option<Duration>,
    /// Generic cap for byte-preserving raw proxy traffic. `None` resolves to one streaming request
    /// in `serve`; the standalone `proxy` command remains opt-in for compatibility.
    pub raw_proxy_max_concurrent_requests: Option<usize>,
    /// How long a raw generation may wait for a slot or for managed execution before 429/503.
    /// `None` falls back to the runtime file, then 10s.
    pub raw_queue_wait: Option<Duration>,
    /// Optional versioned, atomically replaced adaptive-feedback snapshot.
    pub feedback_file: Option<PathBuf>,
    /// Optional bearer token protecting both control and Ollama-compatible routes.
    pub auth_token: Option<String>,
    /// Explicit opt-in for a non-loopback listener. Requires `auth_token`.
    pub allow_remote: bool,
    /// Optional runtime config file (TOML), re-read when it changes. `None` falls back to
    /// `FREELLAMA_RUNTIME_CONFIG`.
    pub runtime_config: Option<PathBuf>,
    /// Optional JSON-lines usage ledger. `None` falls back to `FREELLAMA_USAGE_FILE`; without
    /// either, usage totals are kept in memory only.
    pub usage_file: Option<PathBuf>,
}

/// Manual so the bearer token never reaches logs: a set token prints as `"<redacted>"`.
impl std::fmt::Debug for PlatformConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Exhaustive, so a new field must be added here (and considered for redaction).
        let Self {
            resource_governor,
            listen,
            upstream,
            cpu_upstream,
            cpu_models,
            benchmark_report,
            policy_file,
            max_concurrent_tasks,
            cpu_max_concurrent_tasks,
            max_queue_wait,
            max_queued_tasks,
            cpu_max_queued_tasks,
            max_sessions,
            session_ttl,
            raw_proxy_max_concurrent_requests,
            raw_queue_wait,
            feedback_file,
            auth_token,
            allow_remote,
            runtime_config,
            usage_file,
        } = self;
        formatter
            .debug_struct("PlatformConfig")
            .field("resource_governor", resource_governor)
            .field("listen", listen)
            .field("upstream", upstream)
            .field("cpu_upstream", cpu_upstream)
            .field("cpu_models", cpu_models)
            .field("benchmark_report", benchmark_report)
            .field("policy_file", policy_file)
            .field("max_concurrent_tasks", max_concurrent_tasks)
            .field("cpu_max_concurrent_tasks", cpu_max_concurrent_tasks)
            .field("max_queue_wait", max_queue_wait)
            .field("max_queued_tasks", max_queued_tasks)
            .field("cpu_max_queued_tasks", cpu_max_queued_tasks)
            .field("max_sessions", max_sessions)
            .field("session_ttl", session_ttl)
            .field(
                "raw_proxy_max_concurrent_requests",
                raw_proxy_max_concurrent_requests,
            )
            .field("raw_queue_wait", raw_queue_wait)
            .field("feedback_file", feedback_file)
            .field("auth_token", &auth_token.as_ref().map(|_| "<redacted>"))
            .field("allow_remote", allow_remote)
            .field("runtime_config", runtime_config)
            .field("usage_file", usage_file)
            .finish()
    }
}

impl PlatformConfig {
    #[must_use]
    pub fn new(
        listen: impl Into<String>,
        upstream: impl Into<String>,
        benchmark_report: Option<PathBuf>,
        policy_file: Option<PathBuf>,
    ) -> Self {
        Self {
            resource_governor: resources::ResourceGovernor::default(),
            listen: listen.into(),
            upstream: upstream.into(),
            cpu_upstream: None,
            cpu_models: BTreeSet::new(),
            benchmark_report,
            policy_file,
            max_concurrent_tasks: None,
            cpu_max_concurrent_tasks: None,
            max_queue_wait: None,
            max_queued_tasks: None,
            cpu_max_queued_tasks: None,
            max_sessions: None,
            session_ttl: None,
            raw_proxy_max_concurrent_requests: None,
            raw_queue_wait: None,
            feedback_file: None,
            auth_token: None,
            allow_remote: false,
            runtime_config: None,
            usage_file: None,
        }
    }

    /// Read live-tunable settings from `path`; edits are applied without a restart.
    #[must_use]
    pub fn with_runtime_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.runtime_config = Some(path.into());
        self
    }

    /// Append one JSON line per finished managed task to `path` and replay it at startup.
    #[must_use]
    pub fn with_usage_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.usage_file = Some(path.into());
        self
    }

    /// Cap how long a task may queue for admission before being refused.
    ///
    /// Exposed on the config, not env-only, so the refusal path can be tested without mutating
    /// process environment — which Rust 2024 makes `unsafe`, and this crate denies `unsafe`.
    #[must_use]
    pub fn with_max_queue_wait(mut self, wait: Duration) -> Self {
        self.max_queue_wait = Some(wait);
        self
    }

    /// Bound retained managed requests while the primary/GPU pool is saturated.
    #[must_use]
    pub fn with_max_queued_tasks(mut self, max: usize) -> Self {
        self.max_queued_tasks = Some(max.max(1));
        self
    }

    /// Bound retained managed requests while the CPU pool is saturated.
    #[must_use]
    pub fn with_cpu_max_queued_tasks(mut self, max: usize) -> Self {
        self.cpu_max_queued_tasks = Some(max.max(1));
        self
    }

    /// Bound in-memory session-affinity metadata for long-lived agents.
    #[must_use]
    pub fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = Some(max_sessions.max(1));
        self
    }

    /// Expire idle session-affinity metadata. This does not evict an Ollama model or its KV cache.
    #[must_use]
    pub fn with_session_ttl(mut self, ttl: Duration) -> Self {
        self.session_ttl = Some(ttl.max(Duration::from_secs(1)));
        self
    }

    #[must_use]
    pub fn with_raw_proxy_max_concurrent_requests(mut self, max: usize) -> Self {
        self.raw_proxy_max_concurrent_requests = Some(max.max(1));
        self
    }

    /// Bound how long raw passthrough generations wait before being refused (0 = refuse at once).
    #[must_use]
    pub fn with_raw_queue_wait(mut self, wait: Duration) -> Self {
        self.raw_queue_wait = Some(wait);
        self
    }

    /// Bound weighted work admitted to the primary/GPU Ollama backend.
    ///
    /// Size it for the weighted workload and `OLLAMA_NUM_PARALLEL`: ordinary chat costs two units,
    /// so one parallel chat needs two units. Ollama's own default is 1; extra units keep the pipe
    /// full and bound bursts but do not create within-process decoding concurrency. Exposed on the
    /// config (not env-only) so a test or embedding application can set it without mutating the
    /// process environment.
    #[must_use]
    pub fn with_max_concurrent_tasks(mut self, slots: usize) -> Self {
        self.max_concurrent_tasks = Some(slots.max(1));
        self
    }

    /// Bound weighted work admitted to the optional CPU Ollama backend.
    #[must_use]
    pub fn with_cpu_max_concurrent_tasks(mut self, slots: usize) -> Self {
        self.cpu_max_concurrent_tasks = Some(slots.max(1));
        self
    }

    /// Persist bounded adaptive feedback across service restarts.
    #[must_use]
    pub fn with_feedback_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.feedback_file = Some(path.into());
        self
    }

    /// Require a bearer token on every platform and passthrough request.
    #[must_use]
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Permit a non-loopback listener. Validation still requires bearer authentication.
    #[must_use]
    pub fn with_remote_access(mut self, enabled: bool) -> Self {
        self.allow_remote = enabled;
        self
    }

    /// Route the named models through a second, CPU-configured Ollama server.
    ///
    /// Start `upstream` normally and isolate `cpu_upstream` in a second Ollama process. `FreeLlama`
    /// keeps discovery, residency, transition locking, and managed requests aligned to the assigned
    /// backend, and pins CPU-managed runner loads with `num_gpu: 0`.
    #[must_use]
    pub fn with_cpu_backend<I, S>(mut self, upstream: impl Into<String>, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cpu_upstream = Some(upstream.into());
        self.cpu_models = models.into_iter().map(Into::into).collect();
        self
    }

    /// Validate the loopback-only platform boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or non-loopback listener or recursive upstream.
    pub fn validate(&self) -> Result<()> {
        let listen: SocketAddr = self.listen.parse().context("invalid --listen address")?;
        ensure!(
            listen.ip().is_loopback() || (self.allow_remote && self.auth_token.is_some()),
            "non-loopback platform listeners require remote access and bearer authentication"
        );
        if let Some(token) = self.auth_token.as_deref() {
            crate::auth::validate_token(token)?;
        }
        ensure!(
            !self.allow_remote || self.auth_token.is_some(),
            "remote access requires bearer authentication"
        );
        ProxyConfig::new(&self.listen, &self.upstream, self.allow_remote).validate()?;
        if let Some(cpu_upstream) = &self.cpu_upstream {
            ensure!(
                !self.cpu_models.is_empty(),
                "--cpu-upstream requires at least one --cpu-model assignment"
            );
            ensure!(
                !same_ollama_endpoint(&self.upstream, cpu_upstream),
                "CPU and GPU upstreams must be different Ollama instances"
            );
            ProxyConfig::new(&self.listen, cpu_upstream, self.allow_remote).validate()?;
        } else {
            ensure!(
                self.cpu_models.is_empty(),
                "CPU model assignments require a CPU upstream"
            );
        }
        ensure!(
            self.cpu_models.iter().all(|model| !model.trim().is_empty()),
            "CPU model assignments must not be empty"
        );
        Ok(())
    }
}

fn same_ollama_endpoint(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (Url::parse(left), Url::parse(right)) else {
        return false;
    };
    if left.port_or_known_default() != right.port_or_known_default() {
        return false;
    }
    let Some(left_host) = left.host_str().map(crate::net::strip_ipv6_brackets) else {
        return false;
    };
    let Some(right_host) = right.host_str().map(crate::net::strip_ipv6_brackets) else {
        return false;
    };
    left_host == right_host
        || (crate::net::is_loopback_host(left_host) && crate::net::is_loopback_host(right_host))
}

/// Print the effective admission budgets and where each value came from.
pub(crate) fn report_admission_budgets(tunables: &runtime::Tunables) {
    let source = |key: &str| tunables.sources.get(key).copied().unwrap_or("default");
    eprintln!(
        "freellama: admission budgets: GPU {} units ({}) / {} queued ({}), CPU {} units ({}) / {} \
         queued ({}); raw passthrough {} request(s) ({}). `freellama config` shows every effective \
         value and its source.",
        tunables.max_concurrent_tasks,
        source("max_concurrent_tasks"),
        tunables.max_queued_tasks,
        source("max_queued_tasks"),
        tunables.cpu_max_concurrent_tasks,
        source("cpu_max_concurrent_tasks"),
        tunables.cpu_max_queued_tasks,
        source("cpu_max_queued_tasks"),
        tunables.raw_max_concurrent_requests,
        source("raw_max_concurrent_requests"),
    );
}

/// Upper bound on a managed generation forwarded to Ollama. Overridable via
/// `FREELLAMA_TASK_TIMEOUT_SECONDS` — the same name the CLI and the NAPI layer read, so one
/// setting covers every path that can make a model generate.
pub(crate) fn platform_task_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_TASK_TIMEOUT_SECONDS",
        crate::DEFAULT_TASK_TIMEOUT_SECS,
    )
}

/// Discovery calls (`/api/tags`, `/api/ps`, `/api/show`) read small in-memory state and must never
/// inherit the generation-sized budget above.
pub(crate) fn platform_control_timeout() -> Duration {
    crate::timeout_from_env(
        "FREELLAMA_CONTROL_TIMEOUT_SECONDS",
        crate::DEFAULT_CONTROL_TIMEOUT_SECS,
    )
}
