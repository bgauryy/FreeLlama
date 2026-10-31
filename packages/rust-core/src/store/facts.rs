//! What each model revision has shown it needs, keyed by model, digest and fact.

use anyhow::Result;
use rusqlite::params;

use super::{Store, integer, now_ms, unsigned};

impl Store {
    /// Replace a learned per-model value.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub fn set_fact(&self, model: &str, digest: &str, fact: &str, value: u64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO model_facts (model, digest, fact, value, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(model, digest, fact)
             DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![model, digest, fact, integer(value), now_ms()],
        )?;
        Ok(())
    }

    /// Add `delta` to a learned per-model count; every process adds only what it saw.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub fn add_fact(&self, model: &str, digest: &str, fact: &str, delta: u64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO model_facts (model, digest, fact, value, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(model, digest, fact)
             DO UPDATE SET value = value + excluded.value, updated_at = excluded.updated_at",
            params![model, digest, fact, integer(delta), now_ms()],
        )?;
        Ok(())
    }

    /// Every learned per-model fact as `(model, digest, fact, value)`.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn facts(&self) -> Result<Vec<(String, String, String, u64)>> {
        let connection = self.lock();
        let mut query =
            connection.prepare_cached("SELECT model, digest, fact, value FROM model_facts")?;
        let rows = query
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, unsigned(row.get(3)?)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
