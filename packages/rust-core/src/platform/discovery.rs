//! Machine profiling and config loading: the standalone discovery helpers (Ollama-free machine
//! introspection, benchmark/policy file loaders, capability parsing) that the server plane in
//! `mod.rs` calls but that carry no request state of their own.

use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::model_bench::AllModelsReport;

use super::{ModelRequestDefaults, TaskKind};

#[derive(Debug, Clone, Serialize)]
pub struct MachineProfile {
    pub os: String,
    pub architecture: String,
    pub chip: Option<String>,
    pub logical_cpus: usize,
    /// Total physical system memory. This is the portable capacity input used for model-fit
    /// checks; it does not imply that a discrete GPU can address all of it.
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
    /// The host GPU as the OS reports it: Apple accelerator model and core count from the I/O
    /// registry, or NVIDIA name and memory from `nvidia-smi`. `None` when neither is readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuProfile>,
    /// Performance and efficiency core counts on hybrid CPUs (`hw.perflevel0/1.logicalcpu` on
    /// macOS); `None` where the OS does not split them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_cpus: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efficiency_cpus: Option<usize>,
}

/// One GPU as named by its driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuProfile {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Dedicated memory; `None` for unified memory, which `memory_bytes` already counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
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
    /// Set by `freellama policy from-eval --allow-smoke`. A smoke file must not unlock `medium` at runtime.
    #[serde(default)]
    smoke: bool,
    #[serde(default)]
    policies: BTreeMap<TaskKind, TaskPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskPolicy {
    qualified_models: Vec<String>,
    /// Request defaults per exact model tag, applied when routing picks that model for the task.
    #[serde(default)]
    model_options: BTreeMap<String, ModelRequestDefaults>,
}

/// Task policies loaded from the policy file.
#[derive(Debug, Default)]
pub(super) struct Policies {
    /// Quality-qualified models per task, best first.
    pub(super) qualified: BTreeMap<TaskKind, Vec<String>>,
    /// Request defaults per task and exact model tag.
    pub(super) model_options: BTreeMap<TaskKind, BTreeMap<String, ModelRequestDefaults>>,
}

pub(super) fn load_policies(path: Option<&PathBuf>) -> Result<Policies> {
    let Some(path) = path else {
        return Ok(Policies::default());
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
        ensure!(
            policy
                .model_options
                .keys()
                .all(|model| !model.trim().is_empty()),
            "policy {task:?} has model_options for an empty model"
        );
        for (model, options) in &policy.model_options {
            ensure!(
                options.num_predict != Some(0),
                "policy {} model_options for {model:?}: num_predict must be at least 1",
                task.as_str()
            );
        }
    }
    let mut policies = Policies::default();
    for (task, policy) in file.policies {
        policies.qualified.insert(task, policy.qualified_models);
        if !policy.model_options.is_empty() {
            policies.model_options.insert(task, policy.model_options);
        }
    }
    Ok(policies)
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
    let memory_bytes = host_total_memory_bytes();
    let (memory_kind, unified_memory) =
        memory_semantics(std::env::consts::OS, std::env::consts::ARCH);
    MachineProfile {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        chip: cached_chip_name(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        memory_bytes,
        memory_kind,
        unified_memory_bytes: unified_memory.then_some(memory_bytes).flatten(),
        available_disk_bytes: available_disk_bytes(),
        ollama_endpoint: upstream.to_owned(),
        gpu: cached_gpu(),
        performance_cpus: cpu_split().0,
        efficiency_cpus: cpu_split().1,
    }
}

fn cached_gpu() -> Option<GpuProfile> {
    static GPU: OnceLock<Option<GpuProfile>> = OnceLock::new();
    GPU.get_or_init(gpu_profile).clone()
}

#[cfg(target_os = "macos")]
fn gpu_profile() -> Option<GpuProfile> {
    let (_, text) = super::resources::bounded_output(
        "/usr/sbin/ioreg",
        &["-r", "-c", "AGXAccelerator", "-l"],
        COMMAND_TIMEOUT,
    )?;
    parse_apple_gpu(&text)
}

#[cfg(target_os = "linux")]
fn gpu_profile() -> Option<GpuProfile> {
    command_output(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    )
    .and_then(|text| parse_nvidia_gpu(&text))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn gpu_profile() -> Option<GpuProfile> {
    None
}

/// The first Apple accelerator's `model` and `gpu-core-count` registry properties.
#[cfg(any(target_os = "macos", test))]
fn parse_apple_gpu(text: &str) -> Option<GpuProfile> {
    let property = |name: &str| {
        text.lines().find_map(|line| {
            let line = line.trim_start_matches([' ', '\t', '|']);
            let rest = line.strip_prefix(&format!("\"{name}\" = "))?;
            Some(rest.trim().trim_matches('"').to_owned())
        })
    };
    Some(GpuProfile {
        name: property("model").filter(|name| !name.is_empty())?,
        cores: property("gpu-core-count").and_then(|value| value.parse().ok()),
        memory_bytes: None,
    })
}

/// The first line of `nvidia-smi --query-gpu=name,memory.total` (MiB).
#[cfg(any(target_os = "linux", test))]
fn parse_nvidia_gpu(text: &str) -> Option<GpuProfile> {
    let (name, memory) = text.lines().next()?.rsplit_once(',')?;
    Some(GpuProfile {
        name: name.trim().to_owned(),
        cores: None,
        memory_bytes: memory
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|mib| mib.checked_mul(1024 * 1024)),
    })
    .filter(|gpu| !gpu.name.is_empty())
}

#[cfg(target_os = "macos")]
fn cpu_split() -> (Option<usize>, Option<usize>) {
    let level =
        |name: &str| command_output("sysctl", &["-n", name]).and_then(|value| value.parse().ok());
    (
        level("hw.perflevel0.logicalcpu"),
        level("hw.perflevel1.logicalcpu"),
    )
}

#[cfg(not(target_os = "macos"))]
const fn cpu_split() -> (Option<usize>, Option<usize>) {
    (None, None)
}

/// [`machine_profile`] off the async runtime: it may spawn `df`, `sysctl` or `PowerShell`.
pub(crate) async fn machine_profile_off_runtime(upstream: &str) -> MachineProfile {
    let owned = upstream.to_owned();
    tokio::task::spawn_blocking(move || machine_profile(&owned))
        .await
        .unwrap_or_else(|_| {
            let (memory_kind, _) = memory_semantics(std::env::consts::OS, std::env::consts::ARCH);
            MachineProfile {
                os: std::env::consts::OS.to_owned(),
                architecture: std::env::consts::ARCH.to_owned(),
                chip: None,
                logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
                memory_bytes: None,
                memory_kind,
                unified_memory_bytes: None,
                available_disk_bytes: None,
                ollama_endpoint: upstream.to_owned(),
                gpu: None,
                performance_cpus: None,
                efficiency_cpus: None,
            }
        })
}

/// Physical RAM does not change while serve runs, and reading it spawns `sysctl` (macOS) or
/// `PowerShell` (Windows), so the value is read once and cached.
pub fn host_total_memory_bytes() -> Option<u64> {
    static TOTAL: OnceLock<Option<u64>> = OnceLock::new();
    *TOTAL.get_or_init(total_memory_bytes)
}

fn cached_chip_name() -> Option<String> {
    static CHIP: OnceLock<Option<String>> = OnceLock::new();
    CHIP.get_or_init(chip_name).clone()
}

/// Whether CPU and GPU share one physical memory pool (Apple Silicon).
#[must_use]
pub const fn host_has_unified_memory() -> bool {
    memory_semantics(std::env::consts::OS, std::env::consts::ARCH).1
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
        command_output(
            "powershell.exe",
            &[
                "-NoProfile",
                "-Command",
                "(Get-PSDrive -Name (Get-Item .).PSDrive.Name).Free",
            ],
        )
        .and_then(|value| value.parse().ok())
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

/// Deadline for one machine-discovery command; `PowerShell` can take seconds to start.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let (success, text) = super::resources::bounded_output(command, args, COMMAND_TIMEOUT)?;
    success
        .then(|| text.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        GpuProfile, machine_profile, memory_semantics, parse_apple_gpu, parse_linux_cpuinfo,
        parse_linux_meminfo, parse_nvidia_gpu,
    };

    #[test]
    fn gpu_profiles_parse_apple_registry_and_nvidia_smi() {
        let ioreg = "+-o AGXAcceleratorG14X  <class AGXAcceleratorG14X, id 0x1000003f8>\n    {\n      \"gpu-core-count\" = 19\n      \"model\" = \"Apple M2 Pro\"\n    }\n";
        assert_eq!(
            parse_apple_gpu(ioreg),
            Some(GpuProfile {
                name: "Apple M2 Pro".into(),
                cores: Some(19),
                memory_bytes: None
            })
        );
        assert_eq!(parse_apple_gpu("no accelerator"), None);
        assert_eq!(
            parse_nvidia_gpu("NVIDIA GeForce RTX 4090, 24564\nNVIDIA GeForce RTX 4090, 24564\n"),
            Some(GpuProfile {
                name: "NVIDIA GeForce RTX 4090".into(),
                cores: None,
                memory_bytes: Some(24_564 * 1024 * 1024),
            })
        );
        assert_eq!(parse_nvidia_gpu(""), None);
    }

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
