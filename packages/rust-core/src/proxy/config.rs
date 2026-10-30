//! Proxy configuration: defaults, builders and validation.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::Url;
use tokio::sync::RwLock;

use super::retry::default_restart_ollama;
use super::{RawAdmission, RestartAction};
use crate::platform::Telemetry;
use crate::platform::resources::ResourceGovernor;

/// Default upstream idle timeout: the longest gap allowed between bytes from Ollama (including
/// the wait for response headers while a model loads). It is not a total deadline, so a streamed
/// generation that keeps producing tokens is never cut off mid-answer, while a wedged connection
/// still fails instead of blocking the caller indefinitely.
/// Matches Ollama's own `OLLAMA_LOAD_TIMEOUT` (5m), so a cold load of a large model is not cut
/// off by the proxy before Ollama itself would give up.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct ProxyConfig {
    pub(crate) resource_governor: ResourceGovernor,
    pub listen: String,
    pub upstream: String,
    pub allow_remote: bool,
    pub request_timeout: Duration,
    /// Opt-in: attempt one automatic Ollama restart when a request fails with a true
    /// connection-refused error (the process is gone, not just slow or erroring). Off by
    /// default — never restart a system process a caller didn't explicitly ask for.
    pub auto_restart_ollama: bool,
    /// Optional byte-preserving proxy inflight cap. Unlike managed admission this cannot infer
    /// task cost or CPU/GPU placement, so it is intentionally an immediate generic refusal.
    pub max_concurrent_requests: Option<usize>,
    /// How long a raw generation may wait for a free slot and for managed execution to finish
    /// before being refused. Zero (the standalone default) refuses immediately.
    pub raw_queue_wait: Duration,
    pub(crate) execution_lock: Option<Arc<RwLock<()>>>,
    pub(crate) restart_action: RestartAction,
    pub(crate) raw_admission: Option<RawAdmission>,
    pub(crate) telemetry: Option<Telemetry>,
}

impl ProxyConfig {
    #[must_use]
    pub fn new(listen: impl Into<String>, upstream: impl Into<String>, allow_remote: bool) -> Self {
        Self {
            resource_governor: ResourceGovernor::default(),
            listen: listen.into(),
            upstream: upstream.into(),
            allow_remote,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            auto_restart_ollama: false,
            max_concurrent_requests: None,
            raw_queue_wait: Duration::ZERO,
            execution_lock: None,
            restart_action: Arc::new(default_restart_ollama),
            raw_admission: None,
            telemetry: None,
        }
    }

    /// Overrides the upstream idle timeout (default 300s): the longest silence allowed between
    /// bytes, not a total deadline. It also bounds reading the incoming request body.
    #[must_use]
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Enables automatic Ollama restart on a true connection-refused failure (see
    /// `auto_restart_ollama`'s doc comment). Off by default.
    #[must_use]
    pub fn with_auto_restart_ollama(mut self, enabled: bool) -> Self {
        self.auto_restart_ollama = enabled;
        self
    }

    /// Bound simultaneous raw compatibility requests. Off by default to preserve existing proxy
    /// semantics; managed `/tasks` should be preferred for weighted CPU/GPU admission.
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, max: usize) -> Self {
        self.max_concurrent_requests = Some(max.max(1));
        self
    }

    /// Wait up to `wait` for a raw slot (and for managed execution) before refusing.
    #[must_use]
    pub fn with_raw_queue_wait(mut self, wait: Duration) -> Self {
        self.raw_queue_wait = wait;
        self
    }

    /// Use a shared, live-adjustable raw admission instead of a fixed cap. The platform passes
    /// one so a runtime-config reload can change the limit and wait without a restart.
    #[must_use]
    pub fn with_raw_admission(mut self, admission: RawAdmission) -> Self {
        self.raw_admission = Some(admission);
        self
    }

    /// Count raw requests and refusals in the platform's metrics.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Telemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Share host pressure and memory reservations with managed execution.
    #[must_use]
    pub fn with_resource_governor(mut self, governor: ResourceGovernor) -> Self {
        self.resource_governor = governor;
        self
    }

    /// Share a backend's managed execution boundary. Raw mutations may load or unload any model,
    /// so they require exclusive access until their response stream ends. Metadata GET/HEAD
    /// requests remain available while managed execution is active.
    #[must_use]
    pub fn with_execution_lock(mut self, execution_lock: Arc<RwLock<()>>) -> Self {
        self.execution_lock = Some(execution_lock);
        self
    }

    /// Overrides what "restart Ollama" actually does. Production code should rely on the default
    /// (the real macOS restart sequence) and only set `with_auto_restart_ollama(true)`; this
    /// exists so tests can verify the retry-then-restart-then-retry-once-more orchestration
    /// without touching a real system process.
    #[must_use]
    pub fn with_restart_action(mut self, restart_action: RestartAction) -> Self {
        self.restart_action = restart_action;
        self
    }

    /// Validate the safe-by-default proxy boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid addresses, remote exposure without an explicit opt-in,
    /// or a recursive upstream.
    pub fn validate(&self) -> Result<()> {
        let listen: SocketAddr = self.listen.parse().context("invalid --listen address")?;
        ensure!(
            self.allow_remote || listen.ip().is_loopback(),
            "non-loopback listeners require --allow-remote"
        );
        let upstream = Url::parse(&self.upstream).context("invalid --upstream URL")?;
        ensure!(
            matches!(upstream.scheme(), "http" | "https"),
            "upstream must use HTTP or HTTPS"
        );
        ensure!(
            !upstream
                .host_str()
                .is_some_and(crate::net::is_unspecified_host),
            "upstream host must be a concrete address, not 0.0.0.0 or [::]; use 127.0.0.1 for a \
             local Ollama"
        );
        let same_port = upstream.port_or_known_default() == Some(listen.port());
        let same_host = upstream.host_str().is_some_and(|host| {
            // `url` renders IPv6 hosts with brackets while `SocketAddr::ip()` does not; compare
            // bracket-free forms so `[::1]` cannot evade recursive-upstream detection. A wildcard
            // listener (0.0.0.0/::) also answers on every loopback address.
            let host = crate::net::strip_ipv6_brackets(host);
            let listen_ip = listen.ip();
            host.parse::<std::net::IpAddr>() == Ok(listen_ip)
                || ((listen_ip.is_loopback() || listen_ip.is_unspecified())
                    && crate::net::is_loopback_host(host))
        });
        ensure!(
            !(same_host && same_port),
            "upstream points back to the proxy"
        );
        Ok(())
    }
}
