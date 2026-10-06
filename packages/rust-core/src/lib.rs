//! Local Ollama gateway, evidence-aware router, benchmark harness, and server library.

pub mod auth;
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
mod timeouts;

pub use doctor::{
    OllamaCliDiagnostic, doctor, local_conservative_config_posture, max_loaded_models_advisory,
    ollama_config_diagnostics, ollama_env_advisories, parse_macos_thermal_status,
    parse_ollama_cli_version, parse_ollama_process_environment,
};
pub use net::{DEFAULT_OLLAMA_ENDPOINT, DEFAULT_SERVE_ENDPOINT};
pub use ollama_http::url as endpoint_url;
pub(crate) use timeouts::{
    DEFAULT_CONTROL_TIMEOUT_SECS, DEFAULT_TASK_TIMEOUT_SECS, timeout_from_env,
};
