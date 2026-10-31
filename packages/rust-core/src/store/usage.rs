//! The usage ledger: one row per finished managed task or raw generation, never deleted.

use anyhow::Result;
use rusqlite::params;

use super::{Store, integer, unsigned};

/// Usage totals of one model on one UTC day (`YYYY-MM-DD`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyUsage {
    pub day: String,
    pub model: String,
    pub tasks: u64,
    pub errors: u64,
    pub prompt_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub output_tokens: u64,
    pub busy_ms: u64,
    pub queue_wait_ms: u64,
}

impl Store {
    /// Append one usage record (`at` in Unix seconds). History is never deleted: rows are a few
    /// hundred bytes, and the operator owns the file.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub(crate) fn append_usage(&self, at: u64, record: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO usage (at, record) VALUES (?1, ?2)",
            params![integer(at), record],
        )?;
        Ok(())
    }

    /// Usage records at or after `since` (Unix seconds), oldest first.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn usage_since(&self, since: u64) -> Result<Vec<String>> {
        let connection = self.lock();
        let mut query =
            connection.prepare_cached("SELECT record FROM usage WHERE at >= ?1 ORDER BY id")?;
        let rows = query
            .query_map([integer(since)], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Usage totals per UTC day and model over the records at or after `since` (Unix seconds),
    /// computed by the database through the `usage_at` index, so startup reads only the window a
    /// report can show while every row stays on disk. Malformed rows are skipped.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn usage_daily_totals(&self, since: u64) -> Result<Vec<DailyUsage>> {
        let connection = self.lock();
        let mut query = connection.prepare_cached(
            "SELECT date(at, 'unixepoch'), json_extract(record, '$.model'), COUNT(*),
                    SUM(json_extract(record, '$.outcome') != 'ok'),
                    SUM(json_extract(record, '$.prompt_tokens')),
                    SUM(json_extract(record, '$.output_tokens')),
                    SUM(json_extract(record, '$.duration_ms')),
                    SUM(json_extract(record, '$.queue_wait_ms')),
                    SUM(json_extract(record, '$.cached_prompt_tokens'))
             FROM usage
             WHERE at >= ?1 AND json_valid(record) AND json_type(record, '$.model') = 'text'
             GROUP BY 1, 2 ORDER BY 1, 2",
        )?;
        let number = |row: &rusqlite::Row<'_>, index: usize| -> rusqlite::Result<u64> {
            Ok(unsigned(row.get::<_, Option<i64>>(index)?.unwrap_or(0)))
        };
        let rows = query
            .query_map([integer(since)], |row| {
                Ok(DailyUsage {
                    day: row.get(0)?,
                    model: row.get(1)?,
                    tasks: number(row, 2)?,
                    errors: number(row, 3)?,
                    prompt_tokens: number(row, 4)?,
                    output_tokens: number(row, 5)?,
                    busy_ms: number(row, 6)?,
                    queue_wait_ms: number(row, 7)?,
                    cached_prompt_tokens: number(row, 8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Tokens processed per client key since `since` (Unix seconds): output plus the prompt
    /// tokens not reused from a prompt cache.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn client_tokens_since(&self, since: u64) -> Result<Vec<(String, u64)>> {
        let connection = self.lock();
        let mut query = connection.prepare_cached(
            "SELECT json_extract(record, '$.client'),
                    SUM(MAX(COALESCE(json_extract(record, '$.prompt_tokens'), 0)
                            - COALESCE(json_extract(record, '$.cached_prompt_tokens'), 0), 0)
                        + COALESCE(json_extract(record, '$.output_tokens'), 0))
             FROM usage
             WHERE at >= ?1 AND json_valid(record) AND json_type(record, '$.client') = 'text'
             GROUP BY 1",
        )?;
        let rows = query
            .query_map([integer(since)], |row| {
                Ok((
                    row.get(0)?,
                    unsigned(row.get::<_, Option<i64>>(1)?.unwrap_or(0)),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
