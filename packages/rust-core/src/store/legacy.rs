//! A one-time import of the files versions before the shared database kept.

use std::path::Path;

use anyhow::Result;
use rusqlite::params;

use super::{Store, integer, now_ms};

/// What a one-time import of the pre-database files brought in.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LegacyImport {
    pub footprints: usize,
    pub usage: usize,
    pub feedback: bool,
}

impl Store {
    /// Import the files earlier versions kept in `dir` (`feedback.json`, `footprints.json`,
    /// `usage.jsonl.1` then `usage.jsonl`) once per database; later calls return `None`. One
    /// immediate transaction checks the mark, imports and sets it, so two processes starting
    /// together import once, and a failure halfway imports nothing. The files are left in place.
    /// # Errors
    /// Returns an error when the database cannot be written; unreadable files are skipped.
    pub fn import_legacy(&self, dir: &Path) -> Result<Option<LegacyImport>> {
        const MARK: &str = "legacy_import";
        let mut connection = self.lock();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let done: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM snapshots WHERE name = ?1)",
            [MARK],
            |row| row.get(0),
        )?;
        if done {
            return Ok(None);
        }
        let at = now_ms();
        let mut imported = LegacyImport::default();
        if let Ok(body) = std::fs::read_to_string(dir.join("feedback.json"))
            && serde_json::from_str::<serde_json::Value>(&body).is_ok()
        {
            transaction.execute(
                "INSERT INTO snapshots (name, body, updated_at) VALUES ('placement_feedback', ?1, ?2)
                 ON CONFLICT(name) DO NOTHING",
                params![body.trim(), at],
            )?;
            imported.feedback = true;
        }
        if let Ok(body) = std::fs::read_to_string(dir.join("footprints.json"))
            && let Ok(serde_json::Value::Array(samples)) = serde_json::from_str(&body)
        {
            let field = |sample: &serde_json::Value, name: &str| sample[name].as_u64();
            for sample in &samples {
                let (Some(backend), Some(digest), Some(context), Some(bytes)) = (
                    sample["backend"].as_str(),
                    sample["digest"].as_str(),
                    field(sample, "context"),
                    field(sample, "bytes"),
                ) else {
                    continue;
                };
                // Older files lack the settings; such samples never match a live setting and are
                // ignored when sizing, exactly as before.
                transaction.execute(
                    "INSERT INTO footprints
                       (backend, digest, context, num_parallel, kv_cache_percent, bytes, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(backend, digest, context, num_parallel, kv_cache_percent)
                     DO UPDATE SET bytes = MAX(bytes, excluded.bytes)",
                    params![
                        backend,
                        digest,
                        integer(context),
                        integer(field(sample, "num_parallel").unwrap_or(0)),
                        integer(field(sample, "kv_cache_percent").unwrap_or(0)),
                        integer(bytes),
                        at,
                    ],
                )?;
                imported.footprints += 1;
            }
        }
        for name in ["usage.jsonl.1", "usage.jsonl"] {
            let Ok(body) = std::fs::read_to_string(dir.join(name)) else {
                continue;
            };
            for line in body.lines() {
                if let Some(ts) = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|record| record["ts"].as_u64())
                {
                    transaction.execute(
                        "INSERT INTO usage (at, record) VALUES (?1, ?2)",
                        params![integer(ts), line],
                    )?;
                    imported.usage += 1;
                }
            }
        }
        transaction.execute(
            "INSERT INTO snapshots (name, body, updated_at) VALUES (?1, ?2, ?3)",
            params![MARK, serde_json::json!({"from": dir}).to_string(), at],
        )?;
        transaction.commit()?;
        Ok(Some(imported))
    }
}
