//! The residency audit: loads, unloads and evictions with their cause and duration.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{Store, integer, unsigned};

/// One load, unload or switch of a runner, as the audit trail records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResidencyEvent {
    /// Unix milliseconds.
    pub at_ms: u64,
    pub backend: String,
    pub model: String,
    /// `load`, `unload` or `evict`.
    pub event: String,
    /// What caused it: `managed_task`, `warm`, `raw_request`, `model_memory_budget`,
    /// `host_memory`, `gpu_memory`, `caller_unload`.
    pub trigger: String,
    /// How long it took, when measured (Ollama's `load_duration` for a load).
    pub duration_ms: Option<u64>,
    /// Runner bytes loaded or freed, when known.
    pub bytes: Option<u64>,
}

/// Measured loads of one model in a window.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasuredLoads {
    pub model: String,
    pub measured: u64,
    pub average_ms: u64,
    /// Time spent loading this model in the window: what switching to it cost.
    pub total_ms: u64,
    pub last_ms: u64,
}

/// What [`Store::event_summary`] counts.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct EventSummary {
    pub counts: Vec<(String, u64)>,
    pub triggers: Vec<(String, u64)>,
    pub loads: Vec<MeasuredLoads>,
    pub recent_loads: Vec<(String, u64)>,
}

impl Store {
    /// Append one residency event. The audit is never deleted.
    /// # Errors
    /// Returns an error when the database cannot be written.
    pub fn record_event(&self, event: &ResidencyEvent) -> Result<()> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO residency_events
               (at_ms, backend, model, event, trigger, duration_ms, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                integer(event.at_ms),
                event.backend,
                event.model,
                event.event,
                event.trigger,
                event.duration_ms.map(integer),
                event.bytes.map(integer),
            ],
        )?;
        Ok(())
    }

    /// Residency events at or after `since_ms`, newest first, at most `limit`.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn events_since(&self, since_ms: u64, limit: usize) -> Result<Vec<ResidencyEvent>> {
        let connection = self.lock();
        let mut query = connection.prepare_cached(
            "SELECT at_ms, backend, model, event, trigger, duration_ms, bytes
             FROM residency_events WHERE at_ms >= ?1 ORDER BY at_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = query
            .query_map(
                params![integer(since_ms), i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok(ResidencyEvent {
                        at_ms: unsigned(row.get(0)?),
                        backend: row.get(1)?,
                        model: row.get(2)?,
                        event: row.get(3)?,
                        trigger: row.get(4)?,
                        duration_ms: row.get::<_, Option<i64>>(5)?.map(unsigned),
                        bytes: row.get::<_, Option<i64>>(6)?.map(unsigned),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Aggregates of the residency events at or after `since_ms`, computed by the database so a
    /// busy window is counted in full: counts per event kind and per cause, measured loads per
    /// model (count, average, latest), and loads per model at or after `thrash_since_ms`.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn event_summary(&self, since_ms: u64, thrash_since_ms: u64) -> Result<EventSummary> {
        let connection = self.lock();
        let pairs = |sql: &str, since: u64| -> Result<Vec<(String, u64)>> {
            let mut query = connection.prepare_cached(sql)?;
            let rows = query
                .query_map([integer(since)], |row| {
                    Ok((row.get::<_, String>(0)?, unsigned(row.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let counts = pairs(
            "SELECT event, COUNT(*) FROM residency_events WHERE at_ms >= ?1 GROUP BY event",
            since_ms,
        )?;
        let triggers = pairs(
            "SELECT trigger, COUNT(*) FROM residency_events WHERE at_ms >= ?1 GROUP BY trigger",
            since_ms,
        )?;
        let recent_loads = pairs(
            "SELECT model, COUNT(*) FROM residency_events
             WHERE event = 'load' AND at_ms >= ?1 GROUP BY model",
            thrash_since_ms,
        )?;
        let mut query = connection.prepare_cached(
            "SELECT model, COUNT(*), CAST(ROUND(AVG(duration_ms)) AS INTEGER), SUM(duration_ms),
                    (SELECT duration_ms FROM residency_events AS latest
                     WHERE latest.model = events.model AND latest.event = 'load'
                       AND latest.duration_ms IS NOT NULL
                     ORDER BY latest.at_ms DESC, latest.id DESC LIMIT 1)
             FROM residency_events AS events
             WHERE event = 'load' AND duration_ms IS NOT NULL AND at_ms >= ?1
             GROUP BY model",
        )?;
        let loads = query
            .query_map([integer(since_ms)], |row| {
                Ok(MeasuredLoads {
                    model: row.get(0)?,
                    measured: unsigned(row.get(1)?),
                    average_ms: unsigned(row.get(2)?),
                    total_ms: unsigned(row.get(3)?),
                    last_ms: unsigned(row.get(4)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(EventSummary {
            counts,
            triggers,
            loads,
            recent_loads,
        })
    }

    /// Average measured load time per model on `backend`, from the most recent loads.
    /// # Errors
    /// Returns an error when the database cannot be read.
    pub fn load_times(&self, backend: &str) -> Result<Vec<(String, f64)>> {
        let connection = self.lock();
        let mut query = connection.prepare_cached(
            "SELECT model, AVG(duration_ms) FROM
               (SELECT model, duration_ms FROM residency_events
                WHERE backend = ?1 AND event = 'load' AND duration_ms IS NOT NULL
                ORDER BY at_ms DESC LIMIT 1000)
             GROUP BY model",
        )?;
        let rows = query
            .query_map([backend], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
