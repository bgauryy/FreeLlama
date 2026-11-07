//! Local Ollama gateway, evidence-aware router, benchmark harness, and server library.

/// The shipped model catalog: tested picks per job with their evidence. MCP bundles the same file.
pub const MODEL_CATALOG: &str = include_str!("model_catalog.json");

/// Greppable source marker: `freellama-source-commit=<sha>[-dirty]`, or `unknown` without git.
pub const SOURCE_MARKER: &str =
    concat!("freellama-source-commit=", env!("FREELLAMA_SOURCE_COMMIT"));

/// Text of `freellama --version`: the crate version, then the source marker.
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\n",
    "freellama-source-commit=",
    env!("FREELLAMA_SOURCE_COMMIT")
);

pub mod auth;
pub mod capability;
mod doctor;
pub mod model_bench;
#[cfg(feature = "napi")]
#[allow(
    unsafe_code,
    reason = "napi-derive expands to FFI glue; this is the only unsafe boundary"
)]
pub mod napi;
mod net;
mod ollama_http;
pub mod platform;
pub mod proxy;
pub mod serve_client;
pub mod store;
mod timeouts;

pub use doctor::{
    OllamaCliDiagnostic, doctor, local_conservative_config_posture, max_loaded_models_advisory,
    ollama_config_diagnostics, ollama_env_advisories, parse_macos_thermal_status,
    parse_ollama_cli_version, parse_ollama_process_environment,
};
pub use net::{
    DEFAULT_OLLAMA_ENDPOINT, DEFAULT_SERVE_ENDPOINT, build_http_client, system_roots_missing,
};
pub use ollama_http::url as endpoint_url;
pub(crate) use timeouts::{
    DEFAULT_CONTROL_TIMEOUT_SECS, DEFAULT_TASK_TIMEOUT_SECS, timeout_from_env,
};
