//! An Ollama-compatible, byte-preserving HTTP sidecar.

use std::{
    io::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{Router, http::Uri, routing::any};
use reqwest::{Client, Url};
use tokio::sync::Mutex as AsyncMutex;

use crate::platform::Telemetry;
use crate::platform::resources::ResourceGovernor;

mod admission;
mod body_budget;
mod config;
mod forward;
mod hooks;
mod lifecycle;
mod raw_request;
mod retry;
mod systemone;
mod transport;
mod upstream_forward;
mod upstreams;
mod usage;

pub use admission::{RawAdmission, RawRejection};
pub(crate) use body_budget::{BodyBudget, expected_body_bytes};
pub use config::ProxyConfig;
pub use hooks::{
    Caller, ForecastFuture, HookFuture, LeaseFuture, RawFinished, RawForecast, RawModelHooks,
    RawOutcome, RawUsage,
};
pub use retry::RestartAction;
pub(crate) use retry::{MAX_ATTEMPTS, retry_delay, retryable_upstream_status};
pub(crate) use upstreams::validate as validate_upstreams;
pub use upstreams::{OpenAiUpstream, load_upstreams};

/// Establishing a loopback connection is instant; a long connect wait only delays the
/// connection-refused handling that restarts or reports a stopped Ollama.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a raw generation waits for a host memory/pressure hold to clear before a 503 with
/// `retry-after`. Short, because raw callers cannot be queued fairly; managed `/tasks` queue.
const RAW_PRESSURE_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct ProxyState {
    resource_governor: ResourceGovernor,
    client: Client,
    request_body_timeout: Duration,
    upstream: String,
    auto_restart_ollama: bool,
    restart_action: RestartAction,
    last_restart_attempt: Arc<AsyncMutex<Option<Instant>>>,
    admission: Option<RawAdmission>,
    /// Control work cannot occupy generation slots while waiting for queued model users.
    unload_admission: RawAdmission,
    raw_queue_wait: Duration,
    load_lock: Option<Arc<AsyncMutex<()>>>,
    telemetry: Option<Telemetry>,
    hooks: Option<Arc<dyn RawModelHooks>>,
    upstreams: Option<upstreams::Upstreams>,
    max_request_bytes: usize,
    /// Shared with every server in the process: bytes of buffered request bodies.
    body_budget: BodyBudget,
    /// The context Ollama gave each model for a request that set no `num_ctx`, read from
    /// `/api/ps` after such a request (keyed without `:latest`). Where `FreeLlama` cannot derive
    /// Ollama's default (a unified-memory host, whose VRAM tier is unknown), this is what lets a
    /// later request without `num_ctx` count as reusing the loaded runner instead of as a cold
    /// load that plans evictions (measured: about 45 ms on every such request).
    learned_context: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
}

/// The key a model's learned default context is kept under: Ollama reads `m` as `m:latest`.
fn context_key(name: &str) -> &str {
    name.strip_suffix(":latest").unwrap_or(name)
}

/// Resolve an incoming Ollama path against the configured upstream.
///
/// # Errors
///
/// Returns an error if either URL is malformed.
pub fn proxy_target(upstream: &str, path_and_query: &str) -> Result<Url> {
    let mut url = Url::parse(upstream).context("invalid upstream URL")?;
    let incoming: Uri = path_and_query.parse().context("invalid incoming URI")?;
    url.set_path(incoming.path());
    url.set_query(incoming.query());
    Ok(url)
}

/// Run the optional Ollama-compatible sidecar until Ctrl-C.
///
/// # Errors
///
/// Returns an error when configuration, binding, or serving fails.
pub async fn serve(config: ProxyConfig) -> Result<()> {
    config.validate()?;
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind proxy at {}", config.listen))?;
    let app = app(config.clone())?;
    // A closed stdout must not stop the server; the banner is informational.
    let _ = writeln!(
        std::io::stdout(),
        "FreeLlama proxy listening on http://{}",
        config.listen
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serve proxy")
}

/// Build the byte-preserving Ollama proxy router for composition with platform routes.
///
/// # Errors
///
/// Returns an error when the proxy boundary is unsafe or the HTTP client cannot be built.
pub fn app(config: ProxyConfig) -> Result<Router> {
    config.validate()?;
    let state = ProxyState {
        resource_governor: config.resource_governor,
        client: crate::net::build_http_client(|builder| {
            builder
                .connect_timeout(CONNECT_TIMEOUT.min(config.request_timeout))
                .read_timeout(config.request_timeout)
        })
        .context("build upstream HTTP client")?,
        request_body_timeout: config.request_timeout,
        upstream: config.upstream,
        auto_restart_ollama: config.auto_restart_ollama,
        restart_action: config.restart_action,
        last_restart_attempt: Arc::new(AsyncMutex::new(None)),
        admission: config.raw_admission.or_else(|| {
            config
                .max_concurrent_requests
                .map(|max| RawAdmission::new(max, config.raw_queue_wait))
        }),
        unload_admission: RawAdmission::new(1, config.raw_queue_wait),
        raw_queue_wait: config.raw_queue_wait,
        load_lock: config.load_lock,
        telemetry: config.telemetry,
        hooks: config.hooks,
        learned_context: Arc::default(),
        upstreams: (!config.upstreams.is_empty())
            .then(|| upstreams::Upstreams::new(config.upstreams)),
        max_request_bytes: config
            .max_request_bytes
            .unwrap_or_else(crate::net::max_request_bytes),
        body_budget: config.body_budget.unwrap_or_else(BodyBudget::shared),
    };
    Ok(Router::new()
        .fallback(any(forward::forward))
        .with_state(state))
}

async fn shutdown() {
    crate::net::shutdown_signal().await;
}
