//! Live tunables and runtime config file reloads.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::SystemTime,
};

use serde::Serialize;
use serde_json::{Value, json};

use super::{PinnedTunables, RuntimeFile, Tunables, process_env, resolve};

/// Live tunables plus what is needed to re-resolve them.
#[derive(Clone)]
pub(crate) struct RuntimeSettings {
    current: Arc<std::sync::RwLock<Tunables>>,
    pinned: Arc<PinnedTunables>,
    file: Option<Arc<PathBuf>>,
    num_parallel: u64,
    last_reload: Arc<StdMutex<ReloadStatus>>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ReloadStatus {
    #[serde(skip)]
    modified: Option<SystemTime>,
    reloads: u64,
    last_error: Option<String>,
    last_reload_unix: Option<u64>,
}

impl RuntimeSettings {
    pub(crate) fn new(
        pinned: PinnedTunables,
        file: Option<PathBuf>,
        num_parallel: u64,
    ) -> anyhow::Result<Self> {
        let parsed = match file.as_deref() {
            Some(path) if path.exists() => RuntimeFile::load(path)
                .map_err(|error| anyhow::anyhow!("runtime config {}: {error}", path.display()))?,
            _ => RuntimeFile::default(),
        };
        let modified = file
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok()?.modified().ok());
        if let Some(value) = process_env("FREELLAMA_USAGE_QUEUE_CAPACITY") {
            anyhow::ensure!(
                value.trim().parse::<std::num::NonZeroUsize>().is_ok(),
                "FREELLAMA_USAGE_QUEUE_CAPACITY must be a positive integer"
            );
        }
        let tunables = resolve(&pinned, &parsed, num_parallel, &process_env);
        for warning in &tunables.env_warnings {
            eprintln!("freellama: {warning}");
        }
        tunables.scopes.validate()?;
        tunables.warming.validate()?;
        Ok(Self {
            current: Arc::new(std::sync::RwLock::new(tunables)),
            pinned: Arc::new(pinned),
            file: file.map(Arc::new),
            num_parallel,
            last_reload: Arc::new(StdMutex::new(ReloadStatus {
                modified,
                ..ReloadStatus::default()
            })),
        })
    }

    pub(crate) fn get(&self) -> Tunables {
        self.current.read().expect("tunables poisoned").clone()
    }

    pub(crate) fn file(&self) -> Option<&Path> {
        self.file.as_deref().map(PathBuf::as_path)
    }

    /// [`Self::reload`] off the async runtime: it stats and reads the config file.
    pub(crate) async fn reload_off_runtime(&self, force: bool) -> Result<Option<Tunables>, String> {
        let settings = self.clone();
        tokio::task::spawn_blocking(move || settings.reload(force))
            .await
            .unwrap_or_else(|error| Err(format!("runtime config reload task failed: {error}")))
    }

    /// Re-read the file when its modification time changed (or always, with `force`).
    /// Returns `Ok(Some(new))` when the effective values changed. A bad file keeps the previous
    /// values and records the error.
    pub(crate) fn reload(&self, force: bool) -> Result<Option<Tunables>, String> {
        let Some(path) = self.file.as_deref() else {
            return if force {
                Err("no runtime config file configured (set --runtime-config or FREELLAMA_RUNTIME_CONFIG)".into())
            } else {
                Ok(None)
            };
        };
        let modified = std::fs::metadata(path.as_path())
            .ok()
            .and_then(|meta| meta.modified().ok());
        {
            let status = self.last_reload.lock().expect("reload status poisoned");
            if !force && status.modified == modified {
                return Ok(None);
            }
        }
        let parsed = if path.exists() {
            RuntimeFile::load(path)
        } else {
            Ok(RuntimeFile::default())
        };
        let mut status = self.last_reload.lock().expect("reload status poisoned");
        status.modified = modified;
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                let message = error.to_string();
                status.last_error = Some(message.clone());
                return Err(message);
            }
        };
        let next = resolve(&self.pinned, &parsed, self.num_parallel, &process_env);
        if let Err(error) = next
            .scopes
            .validate()
            .and_then(|()| next.warming.validate())
        {
            let message = error.to_string();
            status.last_error = Some(message.clone());
            return Err(message);
        }
        if next.usage_queue_capacity != self.get().usage_queue_capacity {
            let message =
                "usage_queue_capacity is fixed at startup; restart to change it".to_owned();
            status.last_error = Some(message.clone());
            return Err(message);
        }
        status.last_error = None;
        status.reloads += 1;
        status.last_reload_unix = Some(super::super::telemetry::now_seconds());
        drop(status);
        let mut current = self.current.write().expect("tunables poisoned");
        if *current == next {
            return Ok(None);
        }
        *current = next.clone();
        Ok(Some(next))
    }

    pub(crate) fn receipt(&self) -> Value {
        let status = self
            .last_reload
            .lock()
            .expect("reload status poisoned")
            .clone();
        json!({
            "file": self.file.as_deref(),
            "precedence": ["cli", "env", "file", "default"],
            "reload": status,
            "settings": self.get().receipt(),
            "env_warnings": self.get().env_warnings,
        })
    }
}
