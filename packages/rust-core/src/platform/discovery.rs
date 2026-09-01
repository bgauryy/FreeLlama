//! Machine profiling and config loading: the standalone discovery helpers (Ollama-free machine
//! introspection, benchmark/policy file loaders, capability parsing) that the server plane in
//! `mod.rs` calls but that carry no request state of their own.

use std::{collections::BTreeMap, path::PathBuf, process::Command};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::model_bench::{AllModelsReport, Capability};

use super::TaskKind;

#[derive(Debug, Clone, Serialize)]
pub struct MachineProfile {
    pub os: String,
    pub architecture: String,
    pub chip: Option<String>,
    pub logical_cpus: usize,
    /// Total physical system memory. This is the portable capacity input used for model-fit
    /// recommendations; it does not imply that a discrete GPU can address all of it.
    pub memory_bytes: Option<u64>,
    /// `unified` only when the host memory is known to be shared with the accelerator. Other
    /// systems report `system`; GPU VRAM remains Ollama's responsibility and is observed through
    /// `/api/ps` rather than guessed from host RAM.
    pub memory_kind: &'static str,
    /// Backward-compatible Apple-silicon unified-memory field. Consumers should prefer
    /// `memory_bytes`.
    /// It is `None` on hosts where system RAM and accelerator memory are not known to be unified.
    pub unified_memory_bytes: Option<u64>,
    pub available_disk_bytes: Option<u64>,
    pub ollama_endpoint: String,
}

pub(super) fn load_benchmark(
    path: Option<&PathBuf>,
) -> Result<BTreeMap<String, BTreeMap<Capability, f64>>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let bytes =
        std::fs::read(path).with_context(|| format!("read benchmark report {}", path.display()))?;
    let report: AllModelsReport = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse benchmark report {}", path.display()))?;
    let mut output: BTreeMap<String, BTreeMap<Capability, f64>> = BTreeMap::new();
    for (capability, rankings) in report.rankings {
        for entry in rankings {
            if entry.attempted > 0 && entry.passed == entry.attempted {
                output
                    .entry(entry.model)
                    .or_default()
                    .insert(capability, entry.useful_cases_per_hour);
            }
        }
    }
    Ok(output)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    schema_version: u32,
    /// Set by `policy-from-eval --allow-smoke`. A smoke file must not unlock `medium` at runtime.
    #[serde(default)]
    smoke: bool,
    #[serde(default)]
    policies: BTreeMap<TaskKind, TaskPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskPolicy {
    qualified_models: Vec<String>,
}

pub(super) fn load_policies(path: Option<&PathBuf>) -> Result<BTreeMap<TaskKind, Vec<String>>> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read policy file {}", path.display()))?;
    let file: PolicyFile =
        toml::from_str(&text).with_context(|| format!("parse policy file {}", path.display()))?;
    ensure!(
        file.schema_version == 1,
        "unsupported policy schema version"
    );
    ensure!(
        !file.smoke,
        "policy file was generated from a smoke run (fewer than three trials); re-run with more \
         trials before using it as a quality contract — `--allow-smoke` is for inspection, not routing"
    );
    for (task, policy) in &file.policies {
        ensure!(
            !policy.qualified_models.is_empty(),
            "policy {task:?} has no qualified models"
        );
        ensure!(
            policy
                .qualified_models
                .iter()
                .all(|model| !model.trim().is_empty()),
            "policy {task:?} contains an empty model"
        );
    }
    Ok(file
        .policies
        .into_iter()
        .map(|(task, policy)| (task, policy.qualified_models))
        .collect())
}

pub(super) fn parse_capability(value: &str) -> Option<Capability> {
    match value {
        "completion" => Some(Capability::Completion),
        "tools" => Some(Capability::Tools),
        "vision" => Some(Capability::Vision),
        "audio" => Some(Capability::Audio),
        "thinking" => Some(Capability::Thinking),
        "embedding" => Some(Capability::Embedding),
        _ => None,
    }
}

pub fn machine_profile(upstream: &str) -> MachineProfile {
    let memory_bytes = total_memory_bytes();
    let (memory_kind, unified_memory) =
        memory_semantics(std::env::consts::OS, std::env::consts::ARCH);
    MachineProfile {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        chip: chip_name(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        memory_bytes,
        memory_kind,
        unified_memory_bytes: unified_memory.then_some(memory_bytes).flatten(),
        available_disk_bytes: available_disk_bytes(),
        ollama_endpoint: upstream.to_owned(),
    }
}

const fn memory_semantics(os: &str, architecture: &str) -> (&'static str, bool) {
    let unified = matches!(
        (os.as_bytes(), architecture.as_bytes()),
        (b"macos", b"aarch64")
    );
    if unified {
        ("unified", true)
    } else {
        ("system", false)
    }
}

#[cfg(target_os = "macos")]
fn chip_name() -> Option<String> {
    command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
}

#[cfg(target_os = "linux")]
fn chip_name() -> Option<String> {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| parse_linux_cpuinfo(&text))
}

#[cfg(target_os = "windows")]
fn chip_name() -> Option<String> {
    command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)",
        ],
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn chip_name() -> Option<String> {
    command_output("sysctl", &["-n", "hw.model"])
}

#[cfg(target_os = "macos")]
fn total_memory_bytes() -> Option<u64> {
    command_output("sysctl", &["-n", "hw.memsize"]).and_then(|value| value.parse().ok())
}

#[cfg(target_os = "linux")]
fn total_memory_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_linux_meminfo(&text))
}

#[cfg(target_os = "windows")]
fn total_memory_bytes() -> Option<u64> {
    command_output(
        "powershell.exe",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ],
    )
    .and_then(|value| value.parse().ok())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn total_memory_bytes() -> Option<u64> {
    command_output("sysctl", &["-n", "hw.physmem"]).and_then(|value| value.parse().ok())
}

fn available_disk_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        return command_output(
            "powershell.exe",
            &[
                "-NoProfile",
                "-Command",
                "(Get-PSDrive -Name (Get-Item .).PSDrive.Name).Free",
            ],
        )
        .and_then(|value| value.parse().ok());
    }
    #[cfg(not(target_os = "windows"))]
    {
        command_output("df", &["-Pk", "."])
            .and_then(|value| value.lines().last().map(str::to_owned))
            .and_then(|line| line.split_whitespace().nth(3).map(str::to_owned))
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|kilobytes| kilobytes.checked_mul(1_024))
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_meminfo(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix("MemTotal:")?.split_whitespace().next()?;
        value.parse::<u64>().ok()?.checked_mul(1_024)
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_cpuinfo(text: &str) -> Option<String> {
    ["model name", "Hardware", "Processor"]
        .into_iter()
        .find_map(|key| {
            text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == key)
                    .then(|| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            })
        })
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{machine_profile, memory_semantics, parse_linux_cpuinfo, parse_linux_meminfo};

    #[test]
    fn machine_profile_reports_portable_host_capacity() {
        let profile = machine_profile("http://127.0.0.1:11434");
        assert!(profile.logical_cpus > 0);
        assert!(profile.memory_bytes.is_some_and(|bytes| bytes > 0));
        assert!(profile.available_disk_bytes.is_some_and(|bytes| bytes > 0));
        if cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64" {
            assert_eq!(profile.memory_kind, "unified");
            assert_eq!(profile.unified_memory_bytes, profile.memory_bytes);
        } else {
            assert_eq!(profile.memory_kind, "system");
            assert_eq!(profile.unified_memory_bytes, None);
        }
    }

    #[test]
    fn parses_linux_memory_without_assuming_a_machine_size() {
        assert_eq!(
            parse_linux_meminfo("MemTotal:       16384256 kB\nMemFree: 100 kB\n"),
            Some(16_777_478_144)
        );
        assert_eq!(parse_linux_meminfo("MemFree: 100 kB\n"), None);
    }

    #[test]
    fn parses_x86_and_arm_linux_cpu_names() {
        assert_eq!(
            parse_linux_cpuinfo("processor: 0\nmodel name: Example CPU 9000\n"),
            Some("Example CPU 9000".to_owned())
        );
        assert_eq!(
            parse_linux_cpuinfo("processor: 0\nHardware: Example ARM Board\n"),
            Some("Example ARM Board".to_owned())
        );
    }

    #[test]
    fn memory_semantics_cover_os_and_architecture_permutations() {
        let operating_systems = ["macos", "linux", "windows", "freebsd", "unknown"];
        let architectures = ["aarch64", "x86_64", "arm", "x86", "unknown"];
        let mut checked = 0;
        for os in operating_systems {
            for architecture in architectures {
                let expected = if os == "macos" && architecture == "aarch64" {
                    ("unified", true)
                } else {
                    ("system", false)
                };
                assert_eq!(memory_semantics(os, architecture), expected);
                checked += 1;
            }
        }
        assert_eq!(checked, 25);
    }
}
