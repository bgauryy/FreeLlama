//! The residency audit: every load, unload and eviction, with its cause and how long it took.
//!
//! Switching models is the expensive part of running several of them on one machine: a resident
//! model answers at once, a cold one first spends its load time. The audit shows what switching
//! costs here, what forced each unload (the model memory budget, host memory, GPU memory or a
//! caller), and whether runners are being swapped back and forth. With a shared database it
//! covers every `FreeLlama` process on the machine.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};

use crate::store::{ResidencyEvent, Store};

/// Events kept in memory for a server without a database, and for its own recent view.
const RECENT_EVENTS: usize = 256;
/// The status view covers this much history.
const SUMMARY_WINDOW_MS: u64 = 60 * 60 * 1000;
/// Loading one model this many times within `THRASH_WINDOW_MS` marks it as thrashing.
const THRASH_LOADS: usize = 3;
const THRASH_WINDOW_MS: u64 = 10 * 60 * 1000;
/// Events listed individually in the status view.
const LISTED_EVENTS: usize = 20;

/// Where the audit writes and what it remembers.
#[derive(Clone, Default)]
pub(super) struct Audit {
    store: Option<Store>,
    recent: Arc<Mutex<VecDeque<ResidencyEvent>>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// One event to record; `at_ms` is filled in when it is recorded.
pub(super) struct Entry<'a> {
    pub(super) backend: &'a str,
    pub(super) model: &'a str,
    pub(super) event: &'static str,
    pub(super) trigger: &'a str,
    pub(super) duration_ms: Option<u64>,
    pub(super) bytes: Option<u64>,
}

impl Audit {
    pub(super) fn new(store: Option<Store>) -> Self {
        Self {
            store,
            recent: Arc::default(),
        }
    }

    /// Record one event. The database write runs off the async runtime and never blocks or
    /// fails the request that caused it.
    pub(super) fn record(&self, entry: &Entry<'_>) {
        let event = ResidencyEvent {
            at_ms: now_ms(),
            backend: entry.backend.to_owned(),
            model: entry.model.to_owned(),
            event: entry.event.to_owned(),
            trigger: entry.trigger.to_owned(),
            duration_ms: entry.duration_ms,
            bytes: entry.bytes,
        };
        {
            let mut recent = self
                .recent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recent.push_back(event.clone());
            while recent.len() > RECENT_EVENTS {
                recent.pop_front();
            }
        }
        if let Some(store) = self.store.clone() {
            let write = move || {
                static FAILURE_REPORTED: std::sync::Once = std::sync::Once::new();
                if let Err(error) = store.record_event(&event) {
                    FAILURE_REPORTED.call_once(|| {
                        eprintln!("residency audit not saved: {error:#}");
                    });
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn_blocking(write)),
                Err(_) => write(),
            }
        }
    }

    /// The status view of the last hour: counts by kind and cause, measured switch time per
    /// model, and the models loaded so often that they look like they are being swapped back and
    /// forth. With a shared database the database aggregates every process's events in full;
    /// without one this process's recent events are summarized, marked when older ones fell out.
    pub(super) async fn summary(&self) -> Value {
        let now = now_ms();
        let since = now.saturating_sub(SUMMARY_WINDOW_MS);
        let thrash_since = now.saturating_sub(THRASH_WINDOW_MS);
        if let Some(store) = self.store.clone() {
            let read = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    store.event_summary(since, thrash_since)?,
                    store.events_since(since, LISTED_EVENTS)?,
                ))
            })
            .await;
            if let Ok(Ok((summary, recent))) = read {
                return from_database(&summary, &recent);
            }
        }
        let recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let truncated = recent.len() >= RECENT_EVENTS
            && recent.front().is_some_and(|oldest| oldest.at_ms >= since);
        let events: Vec<ResidencyEvent> = recent
            .iter()
            .rev()
            .filter(|event| event.at_ms >= since)
            .cloned()
            .collect();
        drop(recent);
        let mut view = summarize(&events, now, "this_process");
        view["truncated"] = json!(truncated);
        view
    }
}

fn thrashing_rule() -> String {
    format!(
        "{THRASH_LOADS} or more loads of one model within {} minutes; raise the model memory budget, pin the model, or give it its own backend",
        THRASH_WINDOW_MS / 60_000
    )
}

fn from_database(summary: &crate::store::EventSummary, recent: &[ResidencyEvent]) -> Value {
    let pairs = |rows: &[(String, u64)]| {
        rows.iter()
            .map(|(key, count)| (key.clone(), json!(count)))
            .collect::<serde_json::Map<_, _>>()
    };
    let switch_ms: serde_json::Map<String, Value> = summary
        .loads
        .iter()
        .map(|loads| {
            (
                loads.model.clone(),
                json!({"measured_loads": loads.measured, "average_ms": loads.average_ms, "total_ms": loads.total_ms, "last_ms": loads.last_ms}),
            )
        })
        .collect();
    let thrashing: Vec<&str> = summary
        .recent_loads
        .iter()
        .filter(|(_, loads)| usize::try_from(*loads).unwrap_or(usize::MAX) >= THRASH_LOADS)
        .map(|(model, _)| model.as_str())
        .collect();
    json!({
        "window_minutes": SUMMARY_WINDOW_MS / 60_000,
        "source": "shared_database",
        "truncated": false,
        "counts": pairs(&summary.counts),
        "by_trigger": pairs(&summary.triggers),
        "switch_ms": switch_ms,
        "thrashing": thrashing,
        "thrashing_rule": thrashing_rule(),
        "recent": recent,
    })
}

fn summarize(events: &[ResidencyEvent], now: u64, source: &str) -> Value {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    let mut triggers: BTreeMap<&str, u64> = BTreeMap::new();
    let mut loads: BTreeMap<&str, (u64, u64, Option<u64>)> = BTreeMap::new();
    let mut recent_loads: BTreeMap<&str, usize> = BTreeMap::new();
    for event in events {
        *counts.entry(event.event.as_str()).or_default() += 1;
        *triggers.entry(event.trigger.as_str()).or_default() += 1;
        if event.event == "load" {
            if now.saturating_sub(event.at_ms) <= THRASH_WINDOW_MS {
                *recent_loads.entry(event.model.as_str()).or_default() += 1;
            }
            let entry = loads.entry(event.model.as_str()).or_default();
            if let Some(duration) = event.duration_ms {
                entry.0 += 1;
                entry.1 += duration;
                // Newest first: the first measured load seen is the latest.
                entry.2.get_or_insert(duration);
            }
        }
    }
    let switch_ms: BTreeMap<&str, Value> = loads
        .into_iter()
        .filter(|(_, (measured, _, _))| *measured > 0)
        .map(|(model, (measured, total, last))| {
            (
                model,
                json!({"measured_loads": measured, "average_ms": total / measured, "total_ms": total, "last_ms": last}),
            )
        })
        .collect();
    let thrashing: Vec<&str> = recent_loads
        .into_iter()
        .filter(|(_, loads)| *loads >= THRASH_LOADS)
        .map(|(model, _)| model)
        .collect();
    json!({
        "window_minutes": SUMMARY_WINDOW_MS / 60_000,
        "source": source,
        "counts": counts,
        "by_trigger": triggers,
        "switch_ms": switch_ms,
        "thrashing": thrashing,
        "thrashing_rule": thrashing_rule(),
        "recent": events.iter().take(LISTED_EVENTS).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(at_ms: u64, model: &str, kind: &str, duration_ms: Option<u64>) -> ResidencyEvent {
        ResidencyEvent {
            at_ms,
            backend: "gpu".into(),
            model: model.into(),
            event: kind.into(),
            trigger: if kind == "evict" {
                "model_memory_budget".into()
            } else {
                "managed_task".into()
            },
            duration_ms,
            bytes: None,
        }
    }

    #[test]
    fn the_summary_reports_switch_time_causes_and_thrashing() {
        let now = 100 * 60 * 1000;
        // Newest first, as the store returns them.
        let events = [
            event(now - 1_000, "big", "load", Some(6_000)),
            event(now - 2_000, "small", "evict", Some(4)),
            event(now - 3_000, "big", "load", Some(5_000)),
            event(now - 4_000, "big", "load", None),
            event(now - 50 * 60 * 1000, "small", "load", Some(800)),
        ];
        let summary = summarize(&events, now, "this_process");
        assert_eq!(summary["counts"]["load"], 4);
        assert_eq!(summary["counts"]["evict"], 1);
        assert_eq!(summary["by_trigger"]["model_memory_budget"], 1);
        assert_eq!(summary["switch_ms"]["big"]["average_ms"], 5_500);
        assert_eq!(summary["switch_ms"]["big"]["last_ms"], 6_000);
        assert_eq!(summary["switch_ms"]["small"]["measured_loads"], 1);
        assert_eq!(summary["thrashing"], json!(["big"]));
        assert_eq!(summary["recent"][0]["model"], "big");
    }

    #[tokio::test]
    async fn events_reach_the_shared_database_and_other_processes_see_them() {
        let store = Store::in_memory().unwrap();
        let writer = Audit::new(Some(store.clone()));
        writer.record(&Entry {
            backend: "gpu",
            model: "m",
            event: "load",
            trigger: "raw_request",
            duration_ms: Some(900),
            bytes: Some(10),
        });
        let reader = Audit::new(Some(store));
        let mut summary = reader.summary().await;
        for _ in 0..100 {
            if summary["counts"]["load"] == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            summary = reader.summary().await;
        }
        assert_eq!(summary["source"], "shared_database");
        assert_eq!(summary["counts"]["load"], 1, "{summary}");
        assert_eq!(summary["switch_ms"]["m"]["average_ms"], 900);
    }
}
