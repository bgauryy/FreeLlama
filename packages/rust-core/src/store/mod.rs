//! The shared local database: one `SQLite` file under the `FreeLlama` home
//! (`<HOME>/.octocode/freellama/freellama.sqlite` unless `FREELLAMA_HOME` says otherwise).
//!
//! Several `FreeLlama` processes on one machine (an MCP-started `serve`, a CLI `serve`, tests)
//! learn the same facts about the same Ollama: how much memory each runner takes, which backend is
//! faster, what ran, and what loading and unloading cost. Write-ahead logging lets them share one
//! file safely instead of overwriting each other's JSON snapshots. Every table is a cache of
//! measurements or an audit trail, never configuration: losing the file loses history, not
//! behavior.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// The database file inside the home directory.
pub const DATABASE_FILE: &str = "freellama.sqlite";
/// Bumped when a table changes shape; older files are migrated in [`migrate`].
const SCHEMA_VERSION: i64 = 2;
/// Another process holding the write lock is waited for this long before a write fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The `FreeLlama` home: `FREELLAMA_HOME`, else `<HOME>/.octocode/freellama` (`USERPROFILE` on
/// Windows). Configuration and the database live here.
#[must_use]
pub fn default_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("FREELLAMA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let user = if cfg!(target_os = "windows") {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    }
    .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(user).join(".octocode").join("freellama"))
}

/// A handle to the shared database. Clones share one connection; calls block briefly, so async
/// callers run them on a blocking thread.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// One measured runner size, keyed by everything that changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintRow {
    pub backend: String,
    pub digest: String,
    pub context: u64,
    pub num_parallel: u64,
    pub kv_cache_percent: u64,
    pub bytes: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

impl Store {
    /// Open (creating if needed) the database at `path`.
    /// # Errors
    /// Returns an error when the directory or file cannot be created, or the schema is newer
    /// than this build understands.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create FreeLlama home {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open FreeLlama database {}", path.display()))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("enable write-ahead logging")?;
        Self::configure(connection, Some(path.to_owned()))
    }

    /// A private database that lives only as long as this process.
    /// # Errors
    /// Returns an error when `SQLite` cannot allocate the database.
    pub fn in_memory() -> Result<Self> {
        Self::configure(Connection::open_in_memory()?, None)
    }

    fn configure(connection: Connection, path: Option<PathBuf>) -> Result<Self> {
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
        })
    }

    /// The file behind this store; `None` for an in-memory one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The JSON body stored under `name`, if any.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn snapshot(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT body FROM snapshots WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Replace the JSON body stored under `name`.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub fn put_snapshot(&self, name: &str, body: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO snapshots (name, body, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at",
            params![name, body, now_ms()],
        )?;
        Ok(())
    }

    /// Replace the JSON body under `name` with `update(current)`, read and written in one
    /// immediate transaction so a concurrent writer in another process is merged, not lost.
    /// # Errors
    /// Returns an error when the database cannot be read or written, or `update` fails.
    pub fn update_snapshot(
        &self,
        name: &str,
        update: impl FnOnce(Option<&str>) -> Result<String>,
    ) -> Result<()> {
        let mut connection = self.lock();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT body FROM snapshots WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let body = update(current.as_deref())?;
        transaction.execute(
            "INSERT INTO snapshots (name, body, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at",
            params![name, body, now_ms()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Record runner sizes. A size only grows: two processes measuring the same runner keep the
    /// larger measurement, whichever writes last.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub(crate) fn record_footprints(&self, rows: &[FootprintRow]) -> Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO footprints
                   (backend, digest, context, num_parallel, kv_cache_percent, bytes, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(backend, digest, context, num_parallel, kv_cache_percent)
                 DO UPDATE SET bytes = MAX(bytes, excluded.bytes), updated_at = excluded.updated_at",
            )?;
            let at = now_ms();
            for row in rows {
                insert.execute(params![
                    row.backend,
                    row.digest,
                    integer(row.context),
                    integer(row.num_parallel),
                    integer(row.kv_cache_percent),
                    integer(row.bytes),
                    at,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// The `limit` most recently measured runner sizes, oldest first.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn footprints(&self, limit: usize) -> Result<Vec<FootprintRow>> {
        let connection = self.lock();
        let mut query = connection.prepare_cached(
            "SELECT backend, digest, context, num_parallel, kv_cache_percent, bytes FROM
               (SELECT * FROM footprints ORDER BY updated_at DESC LIMIT ?1)
             ORDER BY updated_at ASC",
        )?;
        let rows = query
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(FootprintRow {
                    backend: row.get(0)?,
                    digest: row.get(1)?,
                    context: unsigned(row.get(2)?),
                    num_parallel: unsigned(row.get(3)?),
                    kv_cache_percent: unsigned(row.get(4)?),
                    bytes: unsigned(row.get(5)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Create or upgrade the tables. A file from a newer build is refused rather than rewritten.
fn migrate(connection: &Connection) -> Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    anyhow::ensure!(
        version <= SCHEMA_VERSION,
        "the FreeLlama database has schema {version}, newer than this build's {SCHEMA_VERSION}; \
         upgrade FreeLlama or point FREELLAMA_HOME elsewhere"
    );
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS snapshots (
             name TEXT PRIMARY KEY, body TEXT NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS footprints (
             backend TEXT NOT NULL, digest TEXT NOT NULL, context INTEGER NOT NULL,
             num_parallel INTEGER NOT NULL, kv_cache_percent INTEGER NOT NULL,
             bytes INTEGER NOT NULL, updated_at INTEGER NOT NULL,
             PRIMARY KEY (backend, digest, context, num_parallel, kv_cache_percent));
         CREATE TABLE IF NOT EXISTS usage (
             id INTEGER PRIMARY KEY, at INTEGER NOT NULL, record TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS usage_at ON usage (at);
         CREATE TABLE IF NOT EXISTS residency_events (
             id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, backend TEXT NOT NULL,
             model TEXT NOT NULL, event TEXT NOT NULL, trigger TEXT NOT NULL,
             duration_ms INTEGER, bytes INTEGER);
         CREATE INDEX IF NOT EXISTS residency_events_at ON residency_events (at_ms);
         CREATE INDEX IF NOT EXISTS residency_events_model_event
             ON residency_events (model, event, at_ms);
         CREATE TABLE IF NOT EXISTS model_facts (
             model TEXT NOT NULL, digest TEXT NOT NULL, fact TEXT NOT NULL,
             value INTEGER NOT NULL, updated_at INTEGER NOT NULL,
             PRIMARY KEY (model, digest, fact));",
    )?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

mod events;
mod facts;
mod legacy;
mod usage;

pub use events::{EventSummary, MeasuredLoads, ResidencyEvent};
pub use legacy::LegacyImport;
pub use usage::DailyUsage;

#[cfg(test)]
mod tests;
