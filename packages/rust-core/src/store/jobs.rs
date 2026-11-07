//! Opt-in bounded deferred-job records. Inputs are hashed, never retained. A durable record
//! does not authorize replay: only its original in-process worker may complete it.
use super::{Store, integer, unsigned};
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Limits apply to the whole database, including different server namespaces.
pub(crate) const MAX_DURABLE_JOBS: usize = 1024;
pub(crate) const MAX_DURABLE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct JobRecord {
    pub id: String,
    pub input_hash: String,
    pub owner: String,
    pub receipt: String,
    pub terminal: bool,
    pub expires_at: u64,
}

fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    Ok(JobRecord {
        id: row.get(0)?,
        input_hash: row.get(1)?,
        owner: row.get(2)?,
        receipt: row.get(3)?,
        terminal: row.get(4)?,
        expires_at: unsigned(row.get(5)?),
    })
}

impl Store {
    /// Independent connection: durable job commits must not inherit the usage cache's NORMAL
    /// synchronization setting or change it for unrelated writers.
    pub(crate) fn durable_jobs_handle(&self) -> Result<Self> {
        let path = self
            .path()
            .ok_or_else(|| anyhow::anyhow!("durable jobs require a file-backed database"))?;
        let durable = Self::open(path)?;
        durable.lock().pragma_update(None, "synchronous", "FULL")?;
        Ok(durable)
    }

    /// Atomically reuse an idempotency key or claim a bounded slot before any inference starts.
    pub(crate) fn claim_job(
        &self,
        namespace: &str,
        key: &str,
        job: &JobRecord,
        now: u64,
        limit: usize,
    ) -> Result<Option<JobRecord>> {
        let mut connection = self.lock();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM task_jobs WHERE expires_at <= ?1",
            [integer(now)],
        )?;
        let existing = transaction.query_row(
            "SELECT id,input_hash,owner,receipt,terminal,expires_at FROM task_jobs WHERE namespace=?1 AND request_key=?2",
            params![namespace,key], record).optional()?;
        if existing.is_some() {
            transaction.commit()?;
            return Ok(existing);
        }
        // Keep every unexpired key, including terminal/removed jobs. Dropping a key to admit
        // unrelated work would let a retry execute again inside the retention window.
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM task_jobs", [], |row| row.get(0))?;
        ensure!(
            unsigned(count) < limit as u64,
            "durable task registry is full"
        );
        ensure!(
            job.receipt.len() <= MAX_METADATA_BYTES,
            "durable job metadata exceeds 16 KiB"
        );
        transaction.execute("INSERT INTO task_jobs(namespace,id,request_key,input_hash,owner,receipt,terminal,expires_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![namespace,job.id,key,job.input_hash,job.owner,job.receipt,job.terminal,integer(job.expires_at)])?;
        trim_payloads(&transaction, MAX_DURABLE_BYTES)?;
        transaction.commit()?;
        Ok(None)
    }

    pub(crate) fn finish_job(
        &self,
        namespace: &str,
        id: &str,
        owner: &str,
        receipt: &str,
        expires_at: u64,
    ) -> Result<()> {
        let metadata: Value = serde_json::from_str(receipt)?;
        let mut metadata = metadata
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("invalid durable job receipt"))?;
        metadata.remove("result");
        metadata.remove("error");
        ensure!(
            serde_json::to_vec(&metadata)?.len() <= MAX_METADATA_BYTES,
            "durable job metadata exceeds 16 KiB"
        );
        let mut connection = self.lock();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute("UPDATE task_jobs SET receipt=?1,terminal=1,expires_at=?2 WHERE namespace=?3 AND id=?4 AND owner=?5 AND terminal=0",
            params![receipt,integer(expires_at),namespace,id,owner])?;
        ensure!(
            changed == 1,
            "durable job ownership was lost before completion"
        );
        trim_payloads(&transaction, MAX_DURABLE_BYTES)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn job(&self, namespace: &str, id: &str, now: u64) -> Result<Option<JobRecord>> {
        Ok(self.lock().query_row("SELECT id,input_hash,owner,receipt,terminal,expires_at FROM task_jobs WHERE namespace=?1 AND id=?2 AND expires_at>?3 AND COALESCE(json_extract(receipt,'$.removed'),0)=0",
            params![namespace,id,integer(now)], record).optional()?)
    }

    pub(crate) fn jobs(&self, namespace: &str, now: u64, limit: usize) -> Result<Vec<JobRecord>> {
        let connection = self.lock();
        // Strip payloads in SQLite before transferring/allocating them in the control plane.
        let mut query = connection.prepare("SELECT id,input_hash,owner,json_remove(receipt,'$.result','$.error'),terminal,expires_at FROM task_jobs WHERE namespace=?1 AND expires_at>?2 AND COALESCE(json_extract(receipt,'$.removed'),0)=0 ORDER BY expires_at DESC LIMIT ?3")?;
        Ok(query
            .query_map(
                params![namespace, integer(now), integer(limit as u64)],
                record,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn remove_job(&self, namespace: &str, id: &str) -> Result<()> {
        let mut connection = self.lock();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let receipt: Option<(String, bool)> = transaction
            .query_row(
                "SELECT receipt,terminal FROM task_jobs WHERE namespace=?1 AND id=?2",
                params![namespace, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((receipt, terminal)) = receipt {
            ensure!(
                terminal,
                "cannot remove a durable job without a committed terminal receipt"
            );
            let mut value = evicted_receipt(&receipt, id, "task_job_result_removed")?;
            value["removed"] = json!(true);
            transaction.execute(
                "UPDATE task_jobs SET receipt=?1 WHERE namespace=?2 AND id=?3",
                params![serde_json::to_string(&value)?, namespace, id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn evicted_receipt(receipt: &str, id: &str, code: &str) -> Result<Value> {
    let mut value: Value = serde_json::from_str(receipt)?;
    ensure!(value.is_object(), "invalid durable job receipt");
    let original_status = value["original_status"]
        .as_str()
        .or_else(|| value["status"].as_str())
        .unwrap_or("unknown")
        .to_owned();
    value["original_status"] = json!(original_status);
    value["status"] = json!("failed");
    value["reason"] = json!("retained_payload_unavailable");
    value["result"] = Value::Null;
    value["error"] = json!({"code":code,"job_id":id,"error":"The original job settled, but its retained payload is no longer available. The task was not replayed.","original_status":original_status,"retry_safe":false,"result_recoverable":false});
    Ok(value)
}

/// Reclaim only payloads, never unexpired deduplication keys or execution metadata.
fn trim_payloads(transaction: &rusqlite::Transaction<'_>, budget: usize) -> Result<()> {
    loop {
        let bytes: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(length(CAST(receipt AS BLOB))),0) FROM task_jobs",
            [],
            |row| row.get(0),
        )?;
        if unsigned(bytes) <= budget as u64 {
            return Ok(());
        }
        let victim: Option<(String,String,String)> = transaction.query_row(
            "SELECT namespace,id,receipt FROM task_jobs WHERE terminal=1 AND COALESCE(json_extract(receipt,'$.reason'),'')<>'retained_payload_unavailable' AND length(CAST(receipt AS BLOB))-length(CAST(json_remove(receipt,'$.result','$.error') AS BLOB))>1024 ORDER BY expires_at LIMIT 1",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let (namespace, id, receipt) = victim
            .ok_or_else(|| anyhow::anyhow!("durable job metadata exceeds retained-byte budget"))?;
        let compact =
            serde_json::to_string(&evicted_receipt(&receipt, &id, "task_job_result_evicted")?)?;
        ensure!(
            compact.len() < receipt.len(),
            "durable job payload cannot be compacted within budget"
        );
        transaction.execute(
            "UPDATE task_jobs SET receipt=?1 WHERE namespace=?2 AND id=?3",
            params![compact, namespace, id],
        )?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(id: &str) -> JobRecord {
        JobRecord {
            id: id.into(),
            input_hash: "hash".into(),
            owner: "worker1".into(),
            receipt: "{}".into(),
            terminal: false,
            expires_at: 100,
        }
    }

    #[test]
    fn durable_claims_survive_reopen_and_never_duplicate_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.sqlite");
        let store = Store::open(&path).unwrap();
        assert!(
            store
                .claim_job("ns", "key", &sample("first"), 0, 2)
                .unwrap()
                .is_none()
        );
        let reopened = Store::open(&path).unwrap();
        let existing = reopened
            .claim_job("ns", "key", &sample("second"), 0, 2)
            .unwrap()
            .unwrap();
        assert_eq!(existing.id, "first");
        assert!(
            reopened
                .finish_job("ns", "first", "wrong-owner", "{}", 101)
                .is_err()
        );
        store
            .finish_job("ns", "first", "worker1", "{\"status\":\"completed\"}", 101)
            .unwrap();
        assert!(reopened.job("ns", "first", 1).unwrap().unwrap().terminal);
    }

    #[test]
    fn active_records_are_bounded_and_expired_records_reclaim_capacity() {
        let store = Store::in_memory().unwrap();
        store.claim_job("ns", "a", &sample("a"), 0, 1).unwrap();
        assert!(store.claim_job("ns", "b", &sample("b"), 0, 1).is_err());
        assert!(
            store
                .claim_job("ns", "b", &sample("b"), 100, 1)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.jobs("ns", 0, 2).unwrap().len(), 1);
        assert!(store.remove_job("ns", "b").is_err());
        store.finish_job("ns", "b", "worker1", "{}", 200).unwrap();
        store.remove_job("ns", "b").unwrap();
        assert!(store.jobs("ns", 0, 2).unwrap().is_empty());
    }

    #[test]
    fn durable_handle_uses_full_sync_without_changing_usage_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("sync.sqlite")).unwrap();
        let durable = store.durable_jobs_handle().unwrap();
        let sync = |store: &Store| {
            store
                .lock()
                .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(sync(&store), 1);
        assert_eq!(sync(&durable), 2);
        assert!(Store::in_memory().unwrap().durable_jobs_handle().is_err());
    }

    #[test]
    fn concurrent_claims_share_one_original_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("concurrent.sqlite");
        let first = Store::open(&file).unwrap().durable_jobs_handle().unwrap();
        let second = Store::open(&file).unwrap().durable_jobs_handle().unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let run = |store: Store, id: &'static str, gate: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                gate.wait();
                (
                    id,
                    store.claim_job("ns", "same", &sample(id), 0, 1).unwrap(),
                )
            })
        };
        let a = run(first, "a", gate.clone());
        let b = run(second, "b", gate);
        let [a, b] = [a.join().unwrap(), b.join().unwrap()];
        match (a.1, b.1) {
            (None, Some(existing)) => assert_eq!(existing.id, a.0),
            (Some(existing), None) => assert_eq!(existing.id, b.0),
            _ => panic!("exactly one claim must be new"),
        }
    }

    #[test]
    fn terminal_and_removed_keys_survive_capacity_pressure_until_expiry() {
        let store = Store::in_memory().unwrap();
        store.claim_job("ns", "key", &sample("a"), 0, 1).unwrap();
        store
            .finish_job(
                "ns",
                "a",
                "worker1",
                r#"{"id":"a","status":"completed","result":{"answer":42}}"#,
                100,
            )
            .unwrap();
        assert!(
            store
                .claim_job("other-namespace", "new", &sample("b"), 0, 1)
                .is_err()
        );
        assert_eq!(
            store
                .claim_job("ns", "key", &sample("retry"), 0, 1)
                .unwrap()
                .unwrap()
                .id,
            "a"
        );
        store.remove_job("ns", "a").unwrap();
        assert!(store.job("ns", "a", 0).unwrap().is_none());
        assert!(store.jobs("ns", 0, 10).unwrap().is_empty());
        let removed = store
            .claim_job("ns", "key", &sample("retry"), 0, 1)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&removed.receipt).unwrap();
        assert_eq!(value["error"]["code"], "task_job_result_removed");
        assert_eq!(value["error"]["retry_safe"], false);
        assert!(
            store
                .claim_job("ns", "key", &sample("later"), 100, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn result_budget_trims_payloads_without_losing_keys_and_lists_only_metadata() {
        let store = Store::in_memory().unwrap();
        for id in ["a", "b"] {
            store.claim_job("ns", id, &sample(id), 0, 2).unwrap();
            store
                .finish_job(
                    "ns",
                    id,
                    "worker1",
                    &json!({"id":id,"status":"completed","result":"x".repeat(3000)}).to_string(),
                    100,
                )
                .unwrap();
        }
        {
            let mut connection = store.lock();
            let transaction = connection.transaction().unwrap();
            trim_payloads(&transaction, 4096).unwrap();
            transaction.commit().unwrap();
        }
        let values: Vec<Value> = ["a", "b"]
            .iter()
            .map(|id| {
                serde_json::from_str(&store.job("ns", id, 0).unwrap().unwrap().receipt).unwrap()
            })
            .collect();
        let evicted = values
            .iter()
            .find(|value| value["reason"] == "retained_payload_unavailable")
            .unwrap();
        assert_eq!(evicted["original_status"], "completed");
        assert_eq!(evicted["status"], "failed");
        assert_eq!(evicted["error"]["code"], "task_job_result_evicted");
        assert_eq!(evicted["error"]["retry_safe"], false);
        for id in ["a", "b"] {
            assert!(
                store
                    .claim_job("ns", id, &sample("retry"), 0, 2)
                    .unwrap()
                    .is_some()
            );
        }
        for record in store.jobs("ns", 0, 2).unwrap() {
            let value: Value = serde_json::from_str(&record.receipt).unwrap();
            assert!(value.get("result").is_none());
            assert!(value.get("error").is_none());
        }
    }
}
