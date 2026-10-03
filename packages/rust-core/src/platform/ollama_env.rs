//! The Ollama server's effective configuration, read from the Ollama process where possible.
//!
//! Ollama has no API that reports its own settings, and `FreeLlama`'s environment is a poor proxy:
//! Ollama.app on macOS is launched by launchd and a Linux service by systemd, each with its own
//! environment. Every value here carries the source it came from so callers can see how much to
//! trust it. Sources, strongest first:
//!
//! 1. `process`: the environment of the unique `ollama serve` process matching the endpoint (Linux
//!    `/proc/<pid>/environ`, macOS `ps eww`). Only loopback endpoints are inspected.
//! 2. `launchd`: `launchctl getenv` on macOS, which is what Ollama.app inherits.
//! 3. `freellama_env`: this process's environment (the same shell or unit as Ollama, often).
//! 4. `ollama_default`: Ollama's documented default (`envconfig/config.go`, `server/sched.go`).

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Settings whose values change how `FreeLlama` should admit and size work.
pub(super) const TRACKED: [&str; 8] = [
    "OLLAMA_NUM_PARALLEL",
    "OLLAMA_MAX_LOADED_MODELS",
    "OLLAMA_MAX_QUEUE",
    "OLLAMA_CONTEXT_LENGTH",
    "OLLAMA_KEEP_ALIVE",
    "OLLAMA_FLASH_ATTENTION",
    "OLLAMA_KV_CACHE_TYPE",
    "OLLAMA_GPU_OVERHEAD",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Setting {
    pub(super) value: Option<String>,
    pub(super) source: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct OllamaSettings {
    pub(super) settings: BTreeMap<&'static str, Setting>,
    /// Why process inspection did or did not contribute.
    pub(super) process_inspection: String,
    observed_at: u64,
    process_observed: bool,
}

impl OllamaSettings {
    /// Missing settings are genuinely unset only after an endpoint-attributed process read.
    pub(super) fn comparable_process_settings(&self) -> Option<Value> {
        self.process_observed.then(|| {
            json!({
                "settings": self.settings,
                "process": self.process_inspection,
            })
        })
    }
    fn raw(&self, name: &str) -> Option<&str> {
        self.settings.get(name)?.value.as_deref()
    }

    fn source(&self, name: &str) -> &'static str {
        self.settings
            .get(name)
            .map_or("ollama_default", |setting| setting.source)
    }

    fn positive(&self, name: &str) -> Option<u64> {
        self.raw(name)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
    }

    /// `OLLAMA_NUM_PARALLEL`; Ollama's default is 1 (`envconfig.NumParallel`).
    pub(super) fn num_parallel(&self) -> u64 {
        self.positive("OLLAMA_NUM_PARALLEL").unwrap_or(1)
    }

    /// `OLLAMA_CONTEXT_LENGTH` when set to a positive value; 0 or unset means Ollama picks a
    /// VRAM-tiered default at startup.
    pub(super) fn context_length(&self) -> Option<u64> {
        self.positive("OLLAMA_CONTEXT_LENGTH")
    }

    /// `OLLAMA_GPU_OVERHEAD` in bytes, subtracted from each GPU's total before the tier check.
    pub(super) fn gpu_overhead_bytes(&self) -> u64 {
        self.positive("OLLAMA_GPU_OVERHEAD").unwrap_or(0)
    }

    /// Lookup compatible with `RuntimeHints::from_lookup`.
    pub(super) fn lookup(&self, name: &str) -> Option<String> {
        self.raw(name).map(str::to_owned)
    }

    pub(super) fn receipt(&self) -> Value {
        let settings = TRACKED
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    json!({"value": self.raw(name), "source": self.source(name)}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({
            "observed_at": self.observed_at,
            "observation_scope": "process_snapshot",
            "refresh_interval_seconds": 5,
            "settings": settings,
            "effective": {
                "num_parallel": self.num_parallel(),
                "context_length": self.context_length(),
            },
            "process_inspection": self.process_inspection,
        })
    }
}

/// Resolve each tracked setting from the strongest source that has it.
pub(super) fn probe(endpoint: &str) -> OllamaSettings {
    let (process, process_inspection) = if endpoint_is_loopback(endpoint) {
        process_environment(endpoint)
    } else {
        (
            None,
            "not attempted: endpoint is not loopback, so a local process cannot be attributed to it"
                .to_owned(),
        )
    };
    resolve(
        process.as_ref(),
        launchd_getenv,
        |name| std::env::var(name).ok(),
        process_inspection,
    )
}

/// Fresh endpoint-attributed settings for learning; diagnostic fallback sources are insufficient.
pub(super) fn probe_comparable_process(endpoint: &str) -> Option<Value> {
    if !endpoint_is_loopback(endpoint) {
        return None;
    }
    let (process, inspection) = process_environment(endpoint);
    let process = process?;
    resolve(Some(&process), |_| None, |_| None, inspection).comparable_process_settings()
}

/// Pure resolution step, separated from the OS probes so the precedence is testable.
pub(super) fn resolve(
    process: Option<&BTreeMap<String, String>>,
    launchd: impl Fn(&str) -> Option<String>,
    own_env: impl Fn(&str) -> Option<String>,
    process_inspection: String,
) -> OllamaSettings {
    let clean = |value: Option<String>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let mut settings = BTreeMap::new();
    for name in TRACKED {
        let setting = if let Some(process) = process {
            // An observed process is authoritative: a name missing from it is really unset there,
            // so a value in FreeLlama's own environment must not be substituted.
            Setting {
                value: clean(process.get(name).cloned()),
                source: if process.contains_key(name) {
                    "process"
                } else {
                    "ollama_default"
                },
            }
        } else if let Some(value) = clean(launchd(name)) {
            Setting {
                value: Some(value),
                source: "launchd",
            }
        } else if let Some(value) = clean(own_env(name)) {
            Setting {
                value: Some(value),
                source: "freellama_env",
            }
        } else {
            Setting {
                value: None,
                source: "ollama_default",
            }
        };
        settings.insert(name, setting);
    }
    OllamaSettings {
        settings,
        process_inspection,
        observed_at: super::telemetry::now_seconds(),
        process_observed: process.is_some(),
    }
}

fn endpoint_is_loopback(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

/// Keep only tracked names, so inspection never turns into a generic environment dump.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn tracked_only<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> BTreeMap<String, String> {
    pairs
        .filter(|(name, value)| {
            (TRACKED.contains(name) || *name == "OLLAMA_HOST") && !value.trim().is_empty()
        })
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Parse a NUL-separated `/proc/<pid>/environ` blob.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_proc_environ(bytes: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    tracked_only(text.split('\0').filter_map(|entry| entry.split_once('=')))
}

/// Exact executable/argument recognition; a shell mentioning Ollama is not its server.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn is_ollama_serve(mut args: impl Iterator<Item = impl AsRef<str>>) -> bool {
    args.next().is_some_and(|program| {
        std::path::Path::new(program.as_ref())
            .file_name()
            .is_some_and(|name| name == "ollama")
    }) && args.next().is_some_and(|arg| arg.as_ref() == "serve")
}

/// The host setting is read only for attribution, never included in a generic environment dump.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn matches_endpoint(environment: &BTreeMap<String, String>, endpoint: &str) -> bool {
    let Some(target) = reqwest::Url::parse(endpoint).ok() else {
        return false;
    };
    let host = environment
        .get("OLLAMA_HOST")
        .map_or("http://127.0.0.1:11434", String::as_str);
    let configured = if host.contains("://") {
        host.to_owned()
    } else {
        format!("http://{host}")
    };
    let Some(configured) = reqwest::Url::parse(&configured).ok() else {
        return false;
    };
    let address = |url: &reqwest::Url| {
        url.host_str().and_then(|host| {
            if host == "localhost" {
                Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
            } else {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            }
        })
    };
    let local = match (address(&configured), address(&target)) {
        (Some(bind), Some(target)) if target.is_loopback() => {
            bind == target || (bind.is_unspecified() && bind.is_ipv4() == target.is_ipv4())
        }
        _ => false,
    };
    let port = configured.port().unwrap_or(11434);
    local && Some(port) == target.port_or_known_default()
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn select_process(
    candidates: Vec<(String, BTreeMap<String, String>)>,
    endpoint: &str,
) -> (Option<BTreeMap<String, String>>, String) {
    let mut matching = candidates
        .into_iter()
        .filter(|(_, env)| matches_endpoint(env, endpoint));
    let Some((pid, environment)) = matching.next() else {
        return (
            None,
            "unavailable: no readable ollama serve process matches this endpoint".into(),
        );
    };
    if matching.next().is_some() {
        return (
            None,
            "ambiguous: multiple ollama serve processes match this endpoint".into(),
        );
    }
    (
        Some(environment),
        format!("observed: ollama serve pid {pid} for {endpoint}"),
    )
}

#[cfg(target_os = "linux")]
fn process_environment(endpoint: &str) -> (Option<BTreeMap<String, String>>, String) {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (None, "unavailable: /proc is not readable".to_owned());
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&cmdline);
        if is_ollama_serve(text.split('\0').filter(|arg| !arg.is_empty()))
            && let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ"))
        {
            candidates.push((pid.to_owned(), parse_proc_environ(&bytes)));
        }
    }
    select_process(candidates, endpoint)
}

#[cfg(target_os = "macos")]
fn process_environment(endpoint: &str) -> (Option<BTreeMap<String, String>>, String) {
    use std::time::Duration;
    // A full `ps command` listing exceeds the telemetry output cap on busy hosts and can lose a
    // server near its tail. Query exact process names first, then inspect only those PIDs.
    let Some(listing) = super::resources::bounded_command_with(
        "pgrep",
        &["-x", "ollama"],
        Duration::from_millis(1500),
    ) else {
        return (None, "unavailable: could not list processes".to_owned());
    };
    let mut candidates = Vec::new();
    for pid in listing
        .split_whitespace()
        .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
    {
        if let Some(command) = super::resources::bounded_command_with(
            "ps",
            &["eww", "-p", pid, "-o", "command="],
            Duration::from_millis(1500),
        ) && is_ollama_serve(command.split_whitespace())
        {
            candidates.push((
                pid.to_owned(),
                tracked_only(
                    command
                        .split_whitespace()
                        .filter_map(|token| token.split_once('=')),
                ),
            ));
        }
    }
    select_process(candidates, endpoint)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_environment(_endpoint: &str) -> (Option<BTreeMap<String, String>>, String) {
    (
        None,
        "unsupported: process inspection is implemented for Linux and macOS".to_owned(),
    )
}

#[cfg(target_os = "macos")]
fn launchd_getenv(name: &str) -> Option<String> {
    super::resources::bounded_command_with(
        "launchctl",
        &["getenv", name],
        std::time::Duration::from_millis(500),
    )
}

#[cfg(not(target_os = "macos"))]
fn launchd_getenv(_name: &str) -> Option<String> {
    None
}

/// Ollama's automatic `num_ctx` when neither the request, the Modelfile, nor
/// `OLLAMA_CONTEXT_LENGTH` sets one (`server/routes.go`, "vram-based default context"): total GPU
/// memory minus `OLLAMA_GPU_OVERHEAD` per GPU, with 47/23 GiB thresholds.
#[must_use]
pub(super) fn vram_tier_default_context(total_vram_bytes: u64) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    if total_vram_bytes >= 47 * GIB {
        262_144
    } else if total_vram_bytes >= 23 * GIB {
        32_768
    } else {
        4_096
    }
}

/// Read `num_ctx` from `/api/show`'s `parameters` text (Modelfile `PARAMETER num_ctx N`).
pub(super) fn modelfile_num_ctx(show: &Value) -> Option<u64> {
    show.get("parameters")?
        .as_str()?
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == "num_ctx").then(|| parts.next()?.parse::<u64>().ok())?
        })
        .find(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_commands_and_per_endpoint_processes_are_attributed() {
        assert!(is_ollama_serve("ollama serve".split_whitespace()));
        assert!(is_ollama_serve(
            "/opt/homebrew/bin/ollama serve".split_whitespace()
        ));
        assert!(!is_ollama_serve(
            "sh -c /opt/homebrew/bin/ollama serve".split_whitespace()
        ));
        assert!(!is_ollama_serve("ollama run serve".split_whitespace()));
        assert!(!matches_endpoint(
            &BTreeMap::from([("OLLAMA_HOST".into(), "127.0.0.2:11434".into())]),
            "http://127.0.0.1:11434"
        ));
        assert!(matches_endpoint(
            &BTreeMap::from([("OLLAMA_HOST".into(), "0.0.0.0:11434".into())]),
            "http://127.0.0.1:11434"
        ));
        assert!(matches_endpoint(
            &BTreeMap::from([("OLLAMA_HOST".into(), "[::1]:11434".into())]),
            "http://[::1]:11434"
        ));
        let primary = BTreeMap::from([("OLLAMA_NUM_PARALLEL".into(), "2".into())]);
        let cpu = BTreeMap::from([
            ("OLLAMA_HOST".into(), "127.0.0.1:11436".into()),
            ("OLLAMA_NUM_PARALLEL".into(), "4".into()),
        ]);
        let candidates = vec![("1".into(), primary.clone()), ("2".into(), cpu)];
        assert_eq!(
            select_process(candidates.clone(), "http://localhost:11434").0,
            Some(primary.clone())
        );
        assert_eq!(
            select_process(candidates.clone(), "http://127.0.0.1:11436")
                .0
                .unwrap()["OLLAMA_NUM_PARALLEL"],
            "4"
        );
        assert!(
            select_process(candidates, "http://127.0.0.1:11437")
                .0
                .is_none()
        );
        assert!(
            select_process(
                vec![("1".into(), primary.clone()), ("2".into(), primary)],
                "http://127.0.0.1:11434"
            )
            .0
            .is_none()
        );
    }

    #[test]
    fn observed_process_wins_and_its_unset_names_are_not_backfilled() {
        let process = BTreeMap::from([("OLLAMA_NUM_PARALLEL".to_owned(), "4".to_owned())]);
        let settings = resolve(
            Some(&process),
            |_| Some("9".into()),
            |_| Some("7".into()),
            "observed".into(),
        );
        assert_eq!(settings.num_parallel(), 4);
        assert_eq!(settings.source("OLLAMA_NUM_PARALLEL"), "process");
        assert_eq!(settings.context_length(), None);
        assert_eq!(settings.source("OLLAMA_CONTEXT_LENGTH"), "ollama_default");
    }

    #[test]
    fn without_a_process_launchd_beats_own_environment_and_defaults_follow() {
        let settings = resolve(
            None,
            |name| (name == "OLLAMA_CONTEXT_LENGTH").then(|| "8192".into()),
            |name| (name == "OLLAMA_NUM_PARALLEL").then(|| "2".into()),
            "none".into(),
        );
        assert_eq!(settings.context_length(), Some(8192));
        assert_eq!(settings.source("OLLAMA_CONTEXT_LENGTH"), "launchd");
        assert_eq!(settings.num_parallel(), 2);
        assert_eq!(settings.source("OLLAMA_NUM_PARALLEL"), "freellama_env");
        assert_eq!(settings.source("OLLAMA_MAX_QUEUE"), "ollama_default");
    }

    #[test]
    fn zero_or_garbage_values_fall_back_to_ollama_defaults() {
        let process = BTreeMap::from([
            ("OLLAMA_NUM_PARALLEL".to_owned(), "0".to_owned()),
            ("OLLAMA_CONTEXT_LENGTH".to_owned(), "abc".to_owned()),
        ]);
        let settings = resolve(Some(&process), |_| None, |_| None, String::new());
        assert_eq!(settings.num_parallel(), 1);
        assert_eq!(settings.context_length(), None);
    }

    #[test]
    fn proc_environ_keeps_only_tracked_names() {
        let parsed = parse_proc_environ(
            b"HOME=/root\0OLLAMA_NUM_PARALLEL=2\0SECRET_TOKEN=x\0OLLAMA_CONTEXT_LENGTH=16384\0",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["OLLAMA_NUM_PARALLEL"], "2");
        assert!(!parsed.contains_key("SECRET_TOKEN"));
    }

    #[test]
    fn vram_tiers_match_ollama_thresholds() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(vram_tier_default_context(0), 4_096);
        assert_eq!(vram_tier_default_context(16 * GIB), 4_096);
        assert_eq!(vram_tier_default_context(24 * GIB), 32_768);
        assert_eq!(vram_tier_default_context(48 * GIB), 262_144);
    }

    #[test]
    fn modelfile_num_ctx_is_parsed_from_show_parameters() {
        let show = json!({"parameters": "stop \"<|im_end|>\"\nnum_ctx                        16384\ntemperature 0.7"});
        assert_eq!(modelfile_num_ctx(&show), Some(16_384));
        assert_eq!(
            modelfile_num_ctx(&json!({"parameters": "temperature 1"})),
            None
        );
        assert_eq!(modelfile_num_ctx(&json!({})), None);
    }

    #[test]
    fn only_loopback_endpoints_are_inspected() {
        assert!(endpoint_is_loopback("http://127.0.0.1:11434"));
        assert!(endpoint_is_loopback("http://localhost:11434"));
        assert!(endpoint_is_loopback("http://[::1]:11434"));
        assert!(!endpoint_is_loopback("http://10.0.0.5:11434"));
    }
}
