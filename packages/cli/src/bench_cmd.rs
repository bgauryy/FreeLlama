//! `freellama bench-all`: throughput benchmark of every installed model.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use freellama::model_bench::{BenchConfig, benchmark_all};
use serde::Serialize;

pub(crate) async fn benchmark(
    endpoint: String,
    include: Vec<String>,
    timeout_seconds: u64,
    trials: u32,
    output: PathBuf,
) -> Result<()> {
    let report = benchmark_all(&BenchConfig {
        endpoint,
        include,
        timeout_seconds,
        trials,
    })
    .await?;
    write_json(&output, &report)?;
    crate::emit(serde_json::to_string_pretty(&report)?)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write report {}", path.display()))
}
