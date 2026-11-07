//! Optional durable receipts. Never stores an executable request or resumes a worker.
use super::{PlatformConfig, error::ApiError};
use crate::store::{
    Store,
    jobs::{JobRecord, MAX_DURABLE_JOBS, MAX_METADATA_BYTES},
};
use anyhow::{Result, ensure};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use uuid::Uuid;

static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, error)
        .with_code("task_job_persistence_failed")
}

#[derive(Debug, Clone)]
pub(super) struct JobJournal {
    store: Store,
    namespace: String,
    owner: String,
    workers: Arc<Semaphore>,
    secret: Arc<JobSecret>,
}

/// Request identity and input hashes for one submission.
#[derive(Debug, Clone)]
pub(super) struct Identity {
    pub request_key: String,
    pub input_hash: String,
}

const SECRET_BYTES: usize = 32;

/// Per-installation HMAC key for request identity and input hashes. It lives in a sibling file
/// of the database, never in the `task_jobs` table, so a reader of the database alone cannot
/// confirm a guessed idempotency key or prompt.
pub(super) struct JobSecret([u8; SECRET_BYTES]);

impl std::fmt::Debug for JobSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JobSecret(..)")
    }
}

impl JobSecret {
    /// The secret file beside `database`: `<database file name>.job-key`.
    pub(super) fn path_for(database: &Path) -> PathBuf {
        let mut name = database.file_name().unwrap_or_default().to_os_string();
        name.push(".job-key");
        database.with_file_name(name)
    }

    /// Read the installation secret, creating it atomically on first use. The complete key is
    /// written (mode 0600 on unix) under a private temporary name and then hard-linked into
    /// place, so a concurrent opener sees either no file or the whole key and never replaces it.
    pub(super) fn load_or_create(database: &Path) -> Result<Self> {
        use std::io::Write;
        let path = Self::path_for(database);
        loop {
            match Self::read(&path) {
                Ok(secret) => return Ok(secret),
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(anyhow::anyhow!(error)
                        .context(format!("cannot read job secret {}", path.display())));
                }
                Err(_) => {}
            }
            let mut bytes = [0_u8; SECRET_BYTES];
            // Two v4 UUIDs: 244 bits from the operating system's random source.
            bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
            bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
            let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            let linked = std::fs::hard_link(&temporary, &path);
            let _ = std::fs::remove_file(&temporary);
            match linked {
                Ok(()) => return Ok(Self(bytes)),
                // Lost the creation race: read the winner's key on the next iteration.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn read(path: &Path) -> std::io::Result<Self> {
        let bytes: [u8; SECRET_BYTES] = std::fs::read(path)?.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "job secret file is not 32 bytes",
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self(bytes))
    }

    /// Domain-separated HMAC-SHA256 as lowercase hex.
    fn hash(&self, purpose: &str, value: &[u8]) -> String {
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.0) else {
            unreachable!("HMAC accepts keys of any length")
        };
        mac.update(purpose.as_bytes());
        mac.update(&[0]);
        mac.update(value);
        hex(&mac.finalize().into_bytes())
    }
}

/// Unkeyed SHA-256 hex, used only for the non-secret server-topology namespace.
pub(super) fn hash(value: &[u8]) -> String {
    hex(&Sha256::digest(value))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

impl JobJournal {
    pub(super) fn configured(config: &PlatformConfig) -> Result<Option<Self>> {
        if !config.durable_jobs_enabled()? {
            return Ok(None);
        }
        let store = config
            .store
            .clone()
            .ok_or_else(|| anyhow::anyhow!("durable jobs require a configured database"))?;
        ensure!(
            store.path().is_some(),
            "durable jobs require a file-backed database"
        );
        let secret = JobSecret::load_or_create(
            store
                .path()
                .ok_or_else(|| anyhow::anyhow!("durable jobs require a file-backed database"))?,
        )?;
        let store = store.durable_jobs_handle()?;
        let topology = serde_json::to_vec(&(
            &config.listen,
            &config.upstream,
            &config.cpu_upstream,
            &config.cpu_models,
        ))?;
        Ok(Some(Self {
            store,
            namespace: hash(&topology),
            owner: Uuid::new_v4().to_string(),
            workers: WORKERS.get_or_init(|| Arc::new(Semaphore::new(4))).clone(),
            secret: Arc::new(secret),
        }))
    }

    /// Keyed hashes of a submission's identity (`client` plus the caller's key, or the job id
    /// when none was given) and of its serialized input. Stable across restarts of one
    /// installation.
    pub(super) fn identity<C: Serialize + ?Sized>(
        &self,
        client: &C,
        caller_key: Option<&str>,
        id: &str,
        input: &[u8],
    ) -> serde_json::Result<Identity> {
        let request = serde_json::to_vec(&(client, caller_key.unwrap_or(id)))?;
        Ok(Identity {
            request_key: self.secret.hash("request", &request),
            input_hash: self.secret.hash("input", input),
        })
    }

    pub(super) async fn claim(
        &self,
        id: String,
        identity: Identity,
        receipt: Value,
        expires_at: u64,
    ) -> Result<Option<JobRecord>, ApiError> {
        let journal = self.clone();
        let receipt = serde_json::to_string(&receipt).map_err(internal)?;
        if receipt.len() > MAX_METADATA_BYTES {
            return Err(ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "durable job metadata exceeds 16 KiB",
            )
            .with_code("task_job_metadata_too_large"));
        }
        self.blocking(move || {
            journal.store.claim_job(
                &journal.namespace,
                &identity.request_key,
                &JobRecord {
                    id,
                    input_hash: identity.input_hash,
                    owner: journal.owner,
                    receipt,
                    terminal: false,
                    expires_at,
                },
                super::telemetry::now_seconds(),
                MAX_DURABLE_JOBS,
            )
        })
        .await
    }

    pub(super) async fn finish(&self, id: String, receipt: Value) -> Result<(), ApiError> {
        let journal = self.clone();
        let receipt = serde_json::to_string(&receipt).map_err(internal)?;
        self.blocking(move || {
            journal.store.finish_job(
                &journal.namespace,
                &id,
                &journal.owner,
                &receipt,
                super::telemetry::now_seconds().saturating_add(super::jobs::RESULT_TTL.as_secs()),
            )
        })
        .await
    }

    pub(super) async fn get(&self, id: String) -> Result<Option<Value>, ApiError> {
        let journal = self.clone();
        self.blocking(move || {
            journal
                .store
                .job(&journal.namespace, &id, super::telemetry::now_seconds())
        })
        .await?
        .as_ref()
        .map(recovered_receipt)
        .transpose()
    }

    pub(super) async fn list(&self) -> Result<Vec<Value>, ApiError> {
        let journal = self.clone();
        self.blocking(move || {
            journal.store.jobs(
                &journal.namespace,
                super::telemetry::now_seconds(),
                MAX_DURABLE_JOBS,
            )
        })
        .await?
        .iter()
        .map(recovered_receipt)
        .collect()
    }

    pub(super) async fn remove(&self, id: String) -> Result<(), ApiError> {
        let journal = self.clone();
        self.blocking(move || journal.store.remove_job(&journal.namespace, &id))
            .await
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        // Acquire before spawning. The closure keeps its permit even when its awaiting request
        // is cancelled, so detached database work cannot create an unbounded blocking queue.
        let permit = self
            .workers
            .clone()
            .acquire_owned()
            .await
            .map_err(internal)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation()
        })
        .await
        .map_err(internal)?
        .map_err(internal)
    }
}

pub(super) fn recovered_receipt(record: &JobRecord) -> Result<Value, ApiError> {
    let mut receipt: Value = serde_json::from_str(&record.receipt).map_err(internal)?;
    if !record.terminal {
        receipt["status"] = json!("failed");
        receipt["reason"] = json!("execution_outcome_unknown");
        receipt["result"] = Value::Null;
        receipt["error"] = json!({"code":"task_job_outcome_unknown","job_id":record.id,"error":"The original worker is not attached to this server. Execution may have occurred; the task was not replayed.","execution_outcome":"unknown","retry_safe":false});
    }
    receipt["recovered"] = json!(true);
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_awaiters_keep_database_workers_bounded_until_operations_finish() {
        let dir = tempfile::tempdir().unwrap();
        let config = PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
            .with_store(Store::open(&dir.path().join("workers.sqlite")).unwrap())
            .with_durable_jobs(true);
        let mut journal = JobJournal::configured(&config).unwrap().unwrap();
        // Isolate the timing check from other tests sharing the production-wide semaphore.
        journal.workers = Arc::new(Semaphore::new(1));
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let first = journal.clone();
        let awaiting = tokio::spawn(async move {
            first
                .blocking(move || {
                    let _ = started.send(());
                    wait.recv_timeout(std::time::Duration::from_secs(2))?;
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        awaiting.abort();
        assert!(awaiting.await.unwrap_err().is_cancelled());
        assert_eq!(journal.workers.available_permits(), 0);
        let second = journal.clone();
        let mut queued = tokio::spawn(async move { second.blocking(|| Ok(42)).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut queued)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(queued.await.unwrap().unwrap(), 42);
        assert_eq!(journal.workers.available_permits(), 1);
    }

    fn journal_at(database: &Path) -> JobJournal {
        let config = PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
            .with_store(Store::open(database).unwrap())
            .with_durable_jobs(true);
        JobJournal::configured(&config).unwrap().unwrap()
    }

    fn identity_of(journal: &JobJournal) -> Identity {
        journal
            .identity(&Some("client"), Some("retry-key"), "job-id", b"input")
            .unwrap()
    }

    #[test]
    fn keyed_hashes_are_stable_across_restart_and_differ_per_installation() {
        let first = tempfile::tempdir().unwrap();
        let database = first.path().join("jobs.sqlite");
        let before = identity_of(&journal_at(&database));
        let after = identity_of(&journal_at(&database));
        assert_eq!(before.request_key, after.request_key);
        assert_eq!(before.input_hash, after.input_hash);
        assert_ne!(before.request_key, before.input_hash);

        let other = tempfile::tempdir().unwrap();
        let elsewhere = identity_of(&journal_at(&other.path().join("jobs.sqlite")));
        assert_ne!(before.request_key, elsewhere.request_key);
        assert_ne!(before.input_hash, elsewhere.input_hash);
    }

    #[test]
    fn keyed_hashes_do_not_equal_plain_sha256_so_database_readers_cannot_test_guesses() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity_of(&journal_at(&dir.path().join("jobs.sqlite")));
        let request = serde_json::to_vec(&(Some("client"), "retry-key")).unwrap();
        assert_ne!(identity.request_key, hash(&request));
        assert_ne!(identity.input_hash, hash(b"input"));
        assert_eq!(identity.request_key.len(), 64);
    }

    #[test]
    fn the_secret_lives_beside_the_database_not_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("jobs.sqlite");
        let journal = journal_at(&database);
        let secret = std::fs::read(JobSecret::path_for(&database)).unwrap();
        assert_eq!(secret.len(), SECRET_BYTES);
        assert_eq!(secret, journal.secret.0);
        let contains = |path: PathBuf| {
            std::fs::read(path)
                .is_ok_and(|bytes| bytes.windows(SECRET_BYTES).any(|window| window == secret))
        };
        assert!(!contains(database.clone()));
        assert!(!contains(database.with_extension("sqlite-wal")));
    }

    #[cfg(unix)]
    #[test]
    fn the_secret_file_is_private_and_loose_permissions_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("jobs.sqlite");
        let before = identity_of(&journal_at(&database));
        let path = JobSecret::path_for(&database);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let after = identity_of(&journal_at(&database));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(before.request_key, after.request_key);
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_malformed_secret_file_is_refused_instead_of_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("jobs.sqlite");
        std::fs::write(JobSecret::path_for(&database), b"short").unwrap();
        let config = PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None)
            .with_store(Store::open(&database).unwrap())
            .with_durable_jobs(true);
        assert!(JobJournal::configured(&config).is_err());
        assert_eq!(
            std::fs::read(JobSecret::path_for(&database)).unwrap(),
            b"short"
        );
    }

    #[test]
    fn recovery_marks_unfinished_work_unknown_without_replaying() {
        let record = JobRecord {
            id: "one".into(),
            input_hash: "hash".into(),
            owner: "old".into(),
            receipt: json!({"id":"one","status":"running"}).to_string(),
            terminal: false,
            expires_at: 1,
        };
        let receipt = recovered_receipt(&record).unwrap();
        assert_eq!(receipt["status"], "failed");
        assert_eq!(receipt["error"]["retry_safe"], false);
        assert_eq!(receipt["error"]["execution_outcome"], "unknown");
        assert_eq!(receipt["error"]["job_id"], "one");
    }
}
