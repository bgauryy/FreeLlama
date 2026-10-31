//! An opt-in CPU-only Ollama that `serve --spawn-cpu-backend` starts, waits for and stops with it.
//!
//! Running a small helper model on the CPU next to GPU work needs a second Ollama process, because
//! one Ollama keeps a single runner profile per model. Without this the operator starts that
//! process by hand (`docs/CPU_GPU_ROUTING.md`). Here `serve` starts it on loopback with the same
//! CPU-forcing environment, and managed CPU requests still carry `num_gpu: 0`, which is what keeps
//! a model off Metal on Apple silicon. The process is owned: it is stopped when `serve` exits, and
//! a port that already answers is refused rather than adopted.
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

/// Environment for a CPU-only Ollama: every GPU runtime hidden, cloud off, one decoding slot.
const CPU_ENVIRONMENT: [(&str, &str); 7] = [
    ("OLLAMA_NO_CLOUD", "1"),
    ("OLLAMA_LLM_LIBRARY", "cpu"),
    ("OLLAMA_NUM_PARALLEL", "1"),
    ("CUDA_VISIBLE_DEVICES", "-1"),
    ("ROCR_VISIBLE_DEVICES", "-1"),
    ("HIP_VISIBLE_DEVICES", "-1"),
    ("GGML_VK_VISIBLE_DEVICES", "-1"),
];

/// How long a starting Ollama may take to answer before `serve` gives up on it.
const START_LIMIT: Duration = Duration::from_secs(60);

pub(crate) struct CpuBackend {
    child: Child,
    pub(crate) endpoint: String,
}

impl Drop for CpuBackend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eprintln!("freellama: stopped the CPU Ollama at {}", self.endpoint);
    }
}

async fn answers(client: &reqwest::Client, endpoint: &str) -> bool {
    client
        .get(format!("{endpoint}/api/version"))
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Start `binary serve` on `127.0.0.1:port` and wait until it answers.
///
/// # Errors
///
/// Returns an error when the port already answers, the binary cannot start, or the process
/// exits or stays silent past the start limit.
pub(crate) async fn spawn(port: u16, binary: &str, log: Option<&Path>) -> Result<CpuBackend> {
    let endpoint = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    if answers(&client, &endpoint).await {
        bail!(
            "{endpoint} already answers as an Ollama server; stop it, or use it with --cpu-upstream {endpoint}"
        );
    }
    let output = || -> Result<Stdio> {
        Ok(match log {
            Some(path) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open CPU Ollama log {}", path.display()))?
                .into(),
            None => Stdio::null(),
        })
    };
    let child = Command::new(binary)
        .arg("serve")
        .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
        .envs(CPU_ENVIRONMENT)
        .stdin(Stdio::null())
        .stdout(output()?)
        .stderr(output()?)
        .spawn()
        .with_context(|| {
            format!("start `{binary} serve` for the CPU backend (install Ollama or pass --ollama-binary)")
        })?;
    let mut backend = CpuBackend { child, endpoint };
    let started = Instant::now();
    loop {
        if let Some(status) = backend
            .child
            .try_wait()
            .context("check the CPU Ollama process")?
        {
            bail!(
                "the CPU Ollama exited before answering ({status}){}",
                log.map_or_else(String::new, |path| format!("; see {}", path.display()))
            );
        }
        if answers(&client, &backend.endpoint).await {
            eprintln!(
                "freellama: started a CPU-only Ollama at {} in {} ms",
                backend.endpoint,
                started.elapsed().as_millis()
            );
            return Ok(backend);
        }
        if started.elapsed() > START_LIMIT {
            bail!(
                "the CPU Ollama did not answer within {} s",
                START_LIMIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_every_gpu_runtime() {
        let names = CPU_ENVIRONMENT.map(|(name, _)| name);
        for name in [
            "OLLAMA_LLM_LIBRARY",
            "CUDA_VISIBLE_DEVICES",
            "ROCR_VISIBLE_DEVICES",
            "GGML_VK_VISIBLE_DEVICES",
        ] {
            assert!(names.contains(&name), "{name}");
        }
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported() {
        let error = spawn(1, "/nonexistent/ollama", None).await.err().unwrap();
        assert!(error.to_string().contains("--ollama-binary"), "{error:#}");
    }
}
