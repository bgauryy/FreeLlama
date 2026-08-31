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

pub(super) fn parse_capability(value: &str) -> Capability {
    match value {
        "completion" => Capability::Completion,
        "tools" => Capability::Tools,
        "vision" => Capability::Vision,
        "audio" => Capability::Audio,
        "thinking" => Capability::Thinking,
        "embedding" => Capability::Embedding,
        _ => Capability::Other,
    }
}

pub(super) fn machine_profile(upstream: &str) -> MachineProfile {
    MachineProfile {
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        chip: command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        unified_memory_bytes: command_output("sysctl", &["-n", "hw.memsize"])
            .and_then(|value| value.parse().ok()),
        available_disk_bytes: command_output("df", &["-Pk", "."])
            .and_then(|value| value.lines().last().map(str::to_owned))
            .and_then(|line| line.split_whitespace().nth(3).map(str::to_owned))
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|kilobytes| kilobytes.checked_mul(1_024)),
        ollama_endpoint: upstream.to_owned(),
    }
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}
