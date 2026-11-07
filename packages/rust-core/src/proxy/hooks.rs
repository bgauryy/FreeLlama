//! What the platform does around raw requests that name a model. The proxy itself keeps no
//! residency or eviction policy; a standalone proxy runs without hooks.

use std::{future::Future, pin::Pin, sync::Arc};

pub use super::usage::RawUsage;

/// The authenticated caller of a request, as the bearer check found it; requests without
/// client keys carry none.
#[derive(Debug, Clone)]
pub struct Caller(pub Arc<str>);

/// A hook's pending work.
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
/// A lifecycle lease retained until the response finishes or its caller disconnects.
pub type LeaseFuture<'a> = Pin<Box<dyn Future<Output = Box<dyn Send>> + Send + 'a>>;
/// Conservative host allocation required for a raw cold load; `None` means unavailable.
pub type ForecastFuture<'a> = Pin<Box<dyn Future<Output = Option<RawForecast>> + Send + 'a>>;

pub struct RawForecast {
    pub required_host_bytes: u64,
    pub context_tokens: Option<u64>,
    pub digest: Option<String>,
}

/// Platform callbacks for raw generations, so traffic that bypasses managed tasks still counts
/// as model demand and still makes room before it loads a model.
pub trait RawModelHooks: Send + Sync {
    /// Before forwarding a generation for `model`, which Ollama does not hold at a compatible
    /// context: make room for it, as a managed load would.
    fn prepare<'a>(&'a self, model: &'a str, context: Option<u64>) -> HookFuture<'a>;
    /// Count `model` as in use until the returned guard drops at the end of the response.
    fn begin(&self, model: &str) -> Box<dyn Send>;
    /// After bounded admission, wait for a claimed unload while retaining `begin`'s guard.
    fn ready<'a>(&'a self, _model: &'a str) -> HookFuture<'a> {
        Box::pin(async {})
    }
    /// Serialize an explicit raw unload with managed/raw use of the same model.
    fn acquire_unload<'a>(&'a self, _model: &'a str) -> LeaseFuture<'a> {
        Box::pin(async { Box::new(()) as Box<dyn Send> })
    }
    /// Forecast a load even when the latest host pressure sample is healthy.
    fn forecast<'a>(&'a self, _model: &'a str, _context: Option<u64>) -> ForecastFuture<'a> {
        Box::pin(async { None })
    }
    /// A compatible runner was observed before the raw response completed. Re-size any queued
    /// same-model demand that was waiting for capacity for a cold load.
    fn runner_ready(&self) {}
    /// A request completed. `reuse_unverified` means initial resident reuse was not verified: the
    /// runner may have been absent, incompatible or unobservable. It does not independently
    /// prove a physical load or a cold disk cache.
    fn served(&self, model: &str, reuse_unverified: bool);
    /// The response to a served generation ended (or its client left) after `elapsed_ms`, with
    /// what it reported using; `reuse_unverified` as for `served`.
    fn finished(&self, finished: RawFinished<'_>) {
        let _ = finished;
    }
    /// Ollama accepted a caller unload (`keep_alive: 0` with nothing to generate).
    /// Accepted by Ollama is not confirmed absent: it does not establish that the runner left or
    /// that physical work stopped; `unload_confirmed` reports a valid absence observation.
    fn unload_accepted(&self, model: &str);
    /// A valid post-request residency observation confirmed the runner is absent.
    fn unload_confirmed(&self, _model: &str) {}
}

/// One finished raw generation.
pub struct RawFinished<'a> {
    /// `gpu` for Ollama, or the name of the OpenAI-compatible upstream that answered.
    pub backend: &'a str,
    pub model: &'a str,
    pub path: &'a str,
    pub caller: Option<&'a str>,
    /// Initial reuse was unverified; absence, incompatible identity, and unknown are distinct
    /// possibilities. This flag must not qualify physical load-cost learning by itself.
    pub reuse_unverified: bool,
    pub usage: RawUsage,
    pub elapsed_ms: u64,
    /// Stream completion is distinct from releasing local leases on error or disconnect.
    pub outcome: RawOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawOutcome {
    Complete,
    UpstreamError,
    Incomplete,
    ClientDropped,
}
