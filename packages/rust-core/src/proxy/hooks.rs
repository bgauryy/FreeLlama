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

/// Platform callbacks for raw generations, so traffic that bypasses managed tasks still counts
/// as model demand and still makes room before it loads a model.
pub trait RawModelHooks: Send + Sync {
    /// Before forwarding a generation for `model`, which Ollama does not hold at a compatible
    /// context: make room for it, as a managed load would.
    fn prepare<'a>(&'a self, model: &'a str, context: Option<u64>) -> HookFuture<'a>;
    /// Count `model` as in use until the returned guard drops at the end of the response.
    fn begin(&self, model: &str) -> Box<dyn Send>;
    /// A request for `model` was served; `loaded` when it found the model not resident.
    fn served(&self, model: &str, loaded: bool);
    /// The response to a served generation ended (or its client left) after `elapsed_ms`, with
    /// what it reported using; `loaded` as for `served`.
    fn finished(&self, finished: RawFinished<'_>) {
        let _ = finished;
    }
    /// A caller unloaded `model` (`keep_alive: 0` with nothing to generate).
    fn unloaded(&self, model: &str);
}

/// One finished raw generation.
pub struct RawFinished<'a> {
    /// `gpu` for Ollama, or the name of the OpenAI-compatible upstream that answered.
    pub backend: &'a str,
    pub model: &'a str,
    pub path: &'a str,
    pub caller: Option<&'a str>,
    pub loaded: bool,
    pub usage: RawUsage,
    pub elapsed_ms: u64,
}
