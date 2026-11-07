//! Bounded `/show` data. Runner observations never enter this cache, and engine-derived
//! capabilities/defaults expire even when the same weights remain installed and frequently used.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 64 * 1024;
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(in crate::platform) struct MetadataCache(Arc<Mutex<Entries>>, Arc<tokio::sync::Semaphore>);

impl Default for MetadataCache {
    fn default() -> Self {
        Self(Arc::default(), Arc::new(tokio::sync::Semaphore::new(4)))
    }
}

#[derive(Default)]
struct Entries {
    values: BTreeMap<(String, String), Entry>,
    bytes: usize,
}

struct Entry {
    show: Arc<Value>,
    bytes: usize,
    saved: Instant,
    used: Instant,
}

impl MetadataCache {
    pub(super) async fn fetch_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.1).acquire_owned().await.ok()
    }

    pub(super) fn get(&self, upstream: &str, digest: Option<&str>) -> Option<Arc<Value>> {
        let key = key(upstream, digest)?;
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.values.get(&key)?.saved.elapsed() >= FRESH_FOR {
            if let Some(removed) = entries.values.remove(&key) {
                entries.bytes -= removed.bytes;
            }
            return None;
        }
        let entry = entries.values.get_mut(&key)?;
        entry.used = Instant::now();
        Some(Arc::clone(&entry.show))
    }

    pub(super) fn insert(&self, upstream: &str, digest: Option<&str>, show: Arc<Value>) {
        let Some(key) = key(upstream, digest) else {
            return;
        };
        // Serialization and metadata reduction happen outside the small synchronous lock.
        let entry_bytes = |show: &Value| {
            serde_json::to_vec(show).ok().map(|encoded| {
                encoded
                    .len()
                    .saturating_add(key.0.len())
                    .saturating_add(key.1.len())
            })
        };
        let Some(mut bytes) = entry_bytes(&show) else {
            return;
        };
        let mut show = show;
        if bytes > MAX_ENTRY_BYTES {
            // Keep only what routing reads so the model is not re-fetched every catalog pass.
            let reduced = Arc::new(routing_projection(&show));
            match entry_bytes(&reduced) {
                Some(reduced_bytes) if reduced_bytes <= MAX_ENTRY_BYTES => {
                    show = reduced;
                    bytes = reduced_bytes;
                }
                _ => return,
            }
        }
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = entries.values.remove(&key) {
            entries.bytes -= previous.bytes;
        }
        // Stale entries cannot be served again; they must not hold budget against live ones.
        let mut live_bytes = 0;
        entries.values.retain(|_, entry| {
            let live = entry.saved.elapsed() < FRESH_FOR;
            if live {
                live_bytes += entry.bytes;
            }
            live
        });
        entries.bytes = live_bytes;
        while entries.values.len() >= MAX_ENTRIES || entries.bytes + bytes > MAX_BYTES {
            let oldest = entries
                .values
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            if let Some(removed) = entries.values.remove(&oldest) {
                entries.bytes -= removed.bytes;
            }
        }
        entries.bytes += bytes;
        entries.values.insert(
            key,
            Entry {
                show,
                bytes,
                saved: Instant::now(),
                used: Instant::now(),
            },
        );
    }
}

fn key(upstream: &str, digest: Option<&str>) -> Option<(String, String)> {
    let digest = digest.filter(|digest| !digest.trim().is_empty() && digest.len() <= 512)?;
    Some((upstream.to_owned(), digest.to_owned()))
}

/// Keep only routing metadata, never tensors, tokenizer vocabularies, templates, or licenses.
/// Complex model-info values remain explicitly nonnumeric so shape estimation cannot mistake a
/// per-layer array for a uniform layout. Unknown primitive architecture fields remain available.
pub(super) fn compact_show(show: Value) -> Option<Value> {
    let Value::Object(mut object) = show else {
        return None;
    };
    let mut compact = serde_json::Map::new();
    if let Some(capabilities) = object.remove("capabilities") {
        let values = capabilities.as_array()?;
        if values.iter().any(|value| !value.is_string()) {
            return None;
        }
        compact.insert("capabilities".into(), capabilities);
    }
    if let Some(parameters) = object.remove("parameters") {
        parameters.as_str()?;
        compact.insert("parameters".into(), parameters);
    }
    if let Some(info) = object.remove("model_info") {
        let Value::Object(info) = info else {
            return None;
        };
        compact.insert(
            "model_info".into(),
            Value::Object(
                info.into_iter()
                    .filter(|(key, _)| !key.starts_with("tokenizer."))
                    .map(|(key, value)| {
                        let value = match value {
                            Value::Array(_) => Value::Array(Vec::new()),
                            Value::Object(_) => Value::Null,
                            other => other,
                        };
                        (key, value)
                    })
                    .collect(),
            ),
        );
    }
    if let Some(thinking) = object.remove("thinking").filter(Value::is_object) {
        compact.insert("thinking".into(), thinking);
    }
    Some(Value::Object(compact))
}

/// Model-info suffixes (after `<architecture>.`) the KV-cache estimate reads as numbers.
const KV_FIELDS: [&str; 6] = [
    "block_count",
    "embedding_length",
    "attention.head_count",
    "attention.head_count_kv",
    "attention.key_length",
    "attention.value_length",
];

/// Fields the KV-cache estimate refuses to model; their presence alone changes the estimate.
fn is_unmodeled_cache_field(suffix: &str) -> bool {
    suffix.starts_with("ssm.")
        || suffix.starts_with("wkv")
        || suffix.contains("recurrent")
        || suffix.contains("sliding_window")
        || suffix.contains("shared_kv")
        || suffix.contains("cross_attention")
}

/// A further reduction of [`compact_show`] output to exactly what catalog routing reads:
/// capabilities, thinking, the `num_ctx` parameter lines, and the architecture, context length and
/// KV-cache shape fields (plus the keys whose presence disables the KV estimate). Everything else
/// is dropped, so a model with unusually large metadata can still be cached.
fn routing_projection(show: &Value) -> Value {
    let mut projected = serde_json::Map::new();
    for key in ["capabilities", "thinking"] {
        if let Some(value) = show.get(key) {
            projected.insert(key.into(), value.clone());
        }
    }
    if let Some(parameters) = show.get("parameters").and_then(Value::as_str) {
        let num_ctx = parameters
            .lines()
            .filter(|line| line.split_whitespace().next() == Some("num_ctx"))
            .collect::<Vec<_>>()
            .join("\n");
        projected.insert("parameters".into(), Value::String(num_ctx));
    }
    if let Some(info) = show.get("model_info").and_then(Value::as_object) {
        let architecture = info.get("general.architecture").and_then(Value::as_str);
        let prefix = architecture.map(|architecture| format!("{architecture}."));
        let kept = info
            .iter()
            .filter(|(key, _)| {
                key.as_str() == "general.architecture"
                    || key.ends_with(".context_length")
                    || crate::platform::context::is_vision_bound_key(key, architecture)
                    || prefix
                        .as_deref()
                        .and_then(|prefix| key.strip_prefix(prefix))
                        .is_some_and(|suffix| {
                            KV_FIELDS.contains(&suffix) || is_unmodeled_cache_field(suffix)
                        })
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        projected.insert("model_info".into(), Value::Object(kept));
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_requires_endpoint_and_nonempty_digest() {
        let cache = MetadataCache::default();
        cache.insert(
            "gpu",
            Some("revision-a"),
            Arc::new(json!({"capabilities":["completion"]})),
        );
        assert!(cache.get("gpu", Some("revision-a")).is_some());
        assert!(cache.get("cpu", Some("revision-a")).is_none());
        assert!(cache.get("gpu", Some("revision-b")).is_none());
        cache.insert("gpu", None, Arc::new(json!({})));
        cache.insert("gpu", Some(""), Arc::new(json!({})));
        assert!(cache.get("gpu", None).is_none());
        assert!(cache.get("gpu", Some("")).is_none());
    }

    #[test]
    fn eviction_respects_entry_and_byte_budgets_and_recent_use() {
        let cache = MetadataCache::default();
        for index in 0..MAX_ENTRIES {
            cache.insert("gpu", Some(&index.to_string()), Arc::new(json!({})));
        }
        assert!(cache.get("gpu", Some("0")).is_some());
        cache.insert("gpu", Some("new"), Arc::new(json!({})));
        assert!(cache.get("gpu", Some("0")).is_some());
        assert!(cache.get("gpu", Some("1")).is_none());
        let large = Arc::new(json!({"parameters":"x".repeat(60 * 1024)}));
        for index in 0..MAX_ENTRIES {
            cache.insert("gpu", Some(&format!("large-{index}")), Arc::clone(&large));
        }
        let entries = cache.0.lock().unwrap();
        assert!(entries.values.len() < MAX_ENTRIES);
        assert!(entries.bytes <= MAX_BYTES);
    }

    #[test]
    fn metadata_irreducible_to_the_entry_budget_is_not_retained() {
        let cache = MetadataCache::default();
        cache.insert(
            "gpu",
            Some("huge"),
            Arc::new(json!({"capabilities":["x".repeat(MAX_ENTRY_BYTES)]})),
        );
        assert!(cache.get("gpu", Some("huge")).is_none());
        assert_eq!(cache.0.lock().unwrap().bytes, 0);
    }

    #[test]
    fn oversized_metadata_is_cached_as_a_routing_projection_with_honest_bytes() {
        use super::super::{
            advertised_context_from_show, estimate_kv_cache_bytes_per_token_f16, split_capabilities,
        };
        let mut info = serde_json::Map::new();
        for (key, value) in [
            ("general.architecture", json!("llama")),
            ("llama.context_length", json!(8192)),
            ("llama.block_count", json!(32)),
            ("llama.attention.head_count", json!(32)),
            ("llama.attention.head_count_kv", json!(8)),
            ("llama.embedding_length", json!(4096)),
        ] {
            info.insert(key.into(), value);
        }
        for index in 0..4000 {
            info.insert(format!("llama.unrelated_{index:05}"), json!("y".repeat(16)));
        }
        let full = json!({
            "capabilities":["completion","tools"],
            "parameters":format!("temperature 1\nnum_ctx 16384\nstop {}", "z".repeat(1024)),
            "thinking":{"values":[false,"low"]},
            "model_info":info,
        });
        assert!(serde_json::to_vec(&full).unwrap().len() > MAX_ENTRY_BYTES);
        let cache = MetadataCache::default();
        cache.insert("gpu", Some("big"), Arc::new(full.clone()));
        let cached = cache.get("gpu", Some("big")).expect("projection is cached");
        let entries = cache.0.lock().unwrap();
        let entry = &entries.values[&("gpu".to_owned(), "big".to_owned())];
        assert_eq!(entries.bytes, entry.bytes);
        assert_eq!(
            entry.bytes,
            serde_json::to_vec(cached.as_ref()).unwrap().len() + "gpu".len() + "big".len()
        );
        assert!(entry.bytes <= MAX_ENTRY_BYTES);
        drop(entries);
        // Every field catalog routing reads gives the same answer from the projection.
        assert_eq!(split_capabilities(&cached), split_capabilities(&full));
        assert_eq!(
            advertised_context_from_show(&cached),
            advertised_context_from_show(&full)
        );
        assert_eq!(
            estimate_kv_cache_bytes_per_token_f16(&cached),
            estimate_kv_cache_bytes_per_token_f16(&full)
        );
        assert!(estimate_kv_cache_bytes_per_token_f16(&cached).is_some());
        assert_eq!(
            crate::platform::ollama_env::modelfile_num_ctx(&cached),
            Some(16_384)
        );
        assert_eq!(cached["thinking"], full["thinking"]);
        assert_eq!(
            cached["model_info"]["general.architecture"],
            full["model_info"]["general.architecture"]
        );
    }

    #[test]
    fn projection_keeps_keys_that_disable_the_kv_estimate() {
        let projected = routing_projection(&json!({"model_info":{
            "general.architecture":"mamba","mamba.block_count":4,"mamba.ssm.state_size":16,
            "mamba.other":1
        }}));
        assert!(
            projected["model_info"]
                .get("mamba.ssm.state_size")
                .is_some()
        );
        assert!(projected["model_info"].get("mamba.other").is_none());
    }

    #[test]
    fn insert_sweeps_expired_entries_before_evicting_live_ones() {
        let cache = MetadataCache::default();
        for index in 0..MAX_ENTRIES {
            cache.insert("gpu", Some(&index.to_string()), Arc::new(json!({})));
        }
        let expired = Instant::now()
            .checked_sub(FRESH_FOR + Duration::from_secs(1))
            .unwrap();
        {
            let mut entries = cache.0.lock().unwrap();
            for (key, entry) in &mut entries.values {
                // Oldest by use is a live entry ("0"); every odd entry is stale.
                if key.1.parse::<usize>().unwrap() % 2 == 1 {
                    entry.saved = expired;
                }
            }
        }
        cache.insert("gpu", Some("new"), Arc::new(json!({})));
        // The least recently used entry was live, so it must survive: stale ones made room.
        assert!(cache.get("gpu", Some("0")).is_some());
        assert!(cache.get("gpu", Some("new")).is_some());
        let entries = cache.0.lock().unwrap();
        assert_eq!(entries.values.len(), MAX_ENTRIES / 2 + 1);
        assert_eq!(
            entries.bytes,
            entries
                .values
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
        );
    }

    #[test]
    fn hot_lru_access_does_not_extend_metadata_freshness() {
        let cache = MetadataCache::default();
        cache.insert("gpu", Some("revision"), Arc::new(json!({})));
        let key = ("gpu".into(), "revision".into());
        let saved = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        cache.0.lock().unwrap().values.get_mut(&key).unwrap().saved = saved;
        for _ in 0..4 {
            assert!(cache.get("gpu", Some("revision")).is_some());
        }
        assert_eq!(cache.0.lock().unwrap().values[&key].saved, saved);
        cache.0.lock().unwrap().values.get_mut(&key).unwrap().saved = Instant::now()
            .checked_sub(FRESH_FOR + Duration::from_secs(1))
            .unwrap();
        assert!(cache.get("gpu", Some("revision")).is_none());
        assert_eq!(cache.0.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn shared_fetch_budget_bounds_independent_discoveries() {
        use std::{
            future::Future as _,
            task::{Context, Waker},
        };
        let cache = MetadataCache::default();
        let other_endpoint = cache.clone();
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(cache.fetch_slot().await.unwrap());
        }
        let mut fifth = std::pin::pin!(other_endpoint.fetch_slot());
        assert!(
            fifth
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        permits.pop();
        let released_slot = fifth.await.unwrap();
        assert_eq!(cache.1.available_permits(), 0);
        drop((permits, released_slot));
        assert_eq!(cache.1.available_permits(), 4);
    }

    #[test]
    fn compact_metadata_preserves_unknown_shapes_and_thinking_without_large_arrays() {
        let show = compact_show(json!({
            "capabilities":["completion","thinking"],
            "parameters":"num_ctx 4096",
            "thinking":{"values":[false,"low","max"],"default":"low"},
            "model_info":{"general.architecture":"llama","llama.attention.head_count":[8,16],"llama.context_length":4096,"tokenizer.ggml.tokens":["large vocabulary"]},
            "tensors":[{"name":"weight"}],"template":"unused","license":"unused"
        })).unwrap();
        assert_eq!(show["thinking"]["default"], "low");
        assert_eq!(show["model_info"]["llama.attention.head_count"], json!([]));
        assert!(show["model_info"].get("tokenizer.ggml.tokens").is_none());
        assert!(show.get("tensors").is_none());
        assert!(compact_show(json!({"capabilities":[false]})).is_none());
        assert!(compact_show(json!(["invalid"])).is_none());
    }
}
