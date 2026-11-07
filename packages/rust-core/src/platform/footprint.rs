//! Bounded empirical runner footprints. No extrapolation across model revisions or contexts.
use super::CatalogModel;
use crate::store::{FootprintRow, Store};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

/// Measured runner sizes, newest last, plus the settings that scale an estimate. With a store,
/// samples survive restarts and are shared with every `FreeLlama` process on the machine, so
/// estimates come from Ollama's own measurements from the start.
#[derive(Default)]
pub(super) struct FootprintHistory {
    samples: VecDeque<Sample>,
    hints: RuntimeHints,
    backend_hints: BTreeMap<String, RuntimeHints>,
    store: Option<Store>,
}

/// Measurements waiting to be written. Taken under the history lock and written after it is
/// released, so a slow disk never blocks routing or other observers. The store keeps the larger
/// size per runner identity, so writes from several processes, in any order, converge.
#[must_use = "a footprint snapshot does nothing until it is written"]
pub(super) struct FootprintSave {
    store: Store,
    rows: Vec<FootprintRow>,
}

impl FootprintSave {
    /// Write off the async runtime. Failures are reported once per process.
    pub(super) async fn persist(self) {
        // A panic in the writer loses one cache write; the next measurement rewrites it.
        let _ = tokio::task::spawn_blocking(move || self.write()).await;
    }

    fn write(self) {
        static SAVE_FAILURE_REPORTED: std::sync::Once = std::sync::Once::new();
        if let Err(error) = self.store.record_footprints(&self.rows) {
            SAVE_FAILURE_REPORTED.call_once(|| {
                eprintln!("footprint history not saved: {error:#}");
            });
        }
    }
}

/// Ollama settings that scale a runner's memory beyond file size + one F16 KV sequence.
///
/// Resolved independently from each backend's observed process environment, with explicit source
/// receipts when only launchd, the `FreeLlama` environment, or documented defaults are available.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RuntimeHints {
    /// `OLLAMA_NUM_PARALLEL`: Ollama allocates one KV cache per parallel slot.
    pub(super) num_parallel: u64,
    /// KV bytes relative to F16: `q8_0` halves and `q4_0` quarters it, only with flash attention.
    pub(super) kv_cache_percent: u64,
}

impl Default for RuntimeHints {
    fn default() -> Self {
        Self {
            num_parallel: 1,
            kv_cache_percent: 100,
        }
    }
}

impl RuntimeHints {
    pub(super) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num_parallel = get("OLLAMA_NUM_PARALLEL")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1);
        let flash_attention = get("OLLAMA_FLASH_ATTENTION")
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"));
        let kv_cache_percent = match get("OLLAMA_KV_CACHE_TYPE").as_deref().map(str::trim) {
            Some("q8_0") if flash_attention => 50,
            Some("q4_0") if flash_attention => 25,
            _ => 100,
        };
        Self {
            num_parallel,
            kv_cache_percent,
        }
    }
}

/// Compute-graph and runtime buffers on top of weights and KV; Ollama's own estimator adds a
/// comparable per-model graph allocation. Kept deliberately conservative.
const GRAPH_MARGIN_PERCENT: u64 = 10;
/// When an architecture's KV shape is unknown (sliding-window, SSM, hybrid), assume the cache
/// needs a quarter of the file size rather than zero.
const UNKNOWN_KV_PERCENT_OF_FILE: u64 = 25;

/// A load estimate from the model file alone, for when nothing else about the model is known:
/// the file, an assumed KV cache and the graph margin, as [`FootprintHistory::requirement`]
/// assumes without KV metadata.
pub(super) const fn file_only_estimate(file_bytes: u64) -> u64 {
    file_bytes
        .saturating_add(file_bytes / 100 * UNKNOWN_KV_PERCENT_OF_FILE)
        .saturating_add(file_bytes / 100 * GRAPH_MARGIN_PERCENT)
}
/// Measurements kept (one per backend, model revision and context size).
const HISTORY_LIMIT: usize = 64;

/// One measurement. The Ollama settings it was taken under are part of its identity: a size
/// measured with one parallel slot or an F16 cache says nothing safe about four slots or `q8_0`,
/// and samples outlive the process that measured them.
struct Sample {
    backend: String,
    digest: String,
    context: u64,
    bytes: u64,
    num_parallel: u64,
    kv_cache_percent: u64,
}

impl Sample {
    fn taken_under(&self, backend: &str, digest: &str, hints: RuntimeHints) -> bool {
        self.backend == backend
            && self.digest == digest
            && self.num_parallel == hints.num_parallel
            && self.kv_cache_percent == hints.kv_cache_percent
    }
}

impl From<FootprintRow> for Sample {
    fn from(row: FootprintRow) -> Self {
        Self {
            backend: row.backend,
            digest: row.digest,
            context: row.context,
            bytes: row.bytes,
            num_parallel: row.num_parallel,
            kv_cache_percent: row.kv_cache_percent,
        }
    }
}

impl From<&Sample> for FootprintRow {
    fn from(sample: &Sample) -> Self {
        Self {
            backend: sample.backend.clone(),
            digest: sample.digest.clone(),
            context: sample.context,
            num_parallel: sample.num_parallel,
            kv_cache_percent: sample.kv_cache_percent,
            bytes: sample.bytes,
        }
    }
}

impl FootprintHistory {
    pub(super) fn with_hints(hints: RuntimeHints) -> Self {
        Self {
            hints,
            ..Self::default()
        }
    }

    pub(super) fn with_backend_hints(mut self, backend: String, hints: RuntimeHints) -> Self {
        self.set_backend_hints(backend, hints);
        self
    }

    pub(super) fn set_backend_hints(&mut self, backend: String, hints: RuntimeHints) {
        self.backend_hints.insert(backend, hints);
    }

    pub(super) fn len(&self) -> usize {
        self.samples.len()
    }

    /// Load and keep saving samples in `store`. An unreadable store starts empty: it is a cache
    /// of measurements, never a source of truth.
    pub(super) fn with_store(mut self, store: Store) -> Self {
        if let Ok(rows) = store.footprints(HISTORY_LIMIT) {
            self.samples = rows.into_iter().map(Sample::from).collect();
        }
        self.store = Some(store);
        self
    }

    /// The measurements to write after the caller releases the history lock.
    fn snapshot(&self) -> Option<FootprintSave> {
        Some(FootprintSave {
            store: self.store.clone()?,
            rows: self.samples.iter().map(FootprintRow::from).collect(),
        })
    }

    /// Learn from every runner Ollama reports as resident (`/api/ps`), including ones loaded by
    /// raw clients, so `FreeLlama`'s estimates converge on Ollama's measured sizes.
    pub(super) fn observe_resident(&mut self, backend: &str, ps: &Value) -> Option<FootprintSave> {
        let mut changed = false;
        for entry in ps["models"].as_array().into_iter().flatten() {
            let digest = entry["digest"].as_str();
            let mut observation = entry.clone();
            observation["status"] = json!("verified");
            changed |= self.record(backend, digest, &observation);
        }
        if changed { self.snapshot() } else { None }
    }

    /// Record one verified observation; a changed history returns a snapshot to persist.
    pub(super) fn observe(
        &mut self,
        backend: &str,
        digest: Option<&str>,
        observation: &Value,
    ) -> Option<FootprintSave> {
        if self.record(backend, digest, observation) {
            self.snapshot()
        } else {
            None
        }
    }

    /// Store one verified measurement; returns whether anything changed.
    fn record(&mut self, backend: &str, digest: Option<&str>, observation: &Value) -> bool {
        let Some(digest) = digest.filter(|digest| !digest.is_empty()) else {
            return false;
        };
        if observation["status"] != "verified" || observation["digest"].as_str() != Some(digest) {
            return false;
        }
        let (Some(context), Some(bytes)) = (
            observation["context_length"].as_u64(),
            observation["size"].as_u64(),
        ) else {
            return false;
        };
        if context == 0 || bytes == 0 {
            return false;
        }
        let hints = self
            .backend_hints
            .get(backend)
            .copied()
            .unwrap_or(self.hints);
        let same = |sample: &Sample| {
            sample.taken_under(backend, digest, hints) && sample.context == context
        };
        let previous = self
            .samples
            .iter()
            .find(|sample| same(sample))
            .map_or(0, |sample| sample.bytes);
        if previous >= bytes {
            return false;
        }
        self.samples.retain(|sample| !same(sample));
        self.samples.push_back(Sample {
            backend: backend.to_owned(),
            digest: digest.to_owned(),
            context,
            bytes: bytes.max(previous),
            num_parallel: hints.num_parallel,
            kv_cache_percent: hints.kv_cache_percent,
        });
        while self.samples.len() > HISTORY_LIMIT {
            self.samples.pop_front();
        }
        true
    }

    pub(super) fn requirement(
        &self,
        backend: &str,
        model: &CatalogModel,
        context: u64,
        current: Option<&Value>,
        host_memory_relevant: bool,
    ) -> Value {
        if !host_memory_relevant {
            return json!({"required_available_bytes":0,"source":"host_memory_not_applicable","exact":false});
        }
        let warm = current.is_some_and(|entry| {
            entry["context_length"]
                .as_u64()
                .is_some_and(|loaded| context > 0 && loaded == context)
                && model.digest.as_ref().is_some_and(|digest| {
                    !digest.is_empty() && entry["digest"].as_str() == Some(digest.as_str())
                })
        });
        if warm {
            return json!({"required_available_bytes":0,"source":"matching_resident_context","exact":false,
                "note":"resident allocation already reflected in host telemetry; runner growth remains possible"});
        }
        let hints = self
            .backend_hints
            .get(backend)
            .copied()
            .unwrap_or(self.hints);
        let digest = model.digest.as_deref().unwrap_or_default();
        let comparable =
            |sample: &&Sample| !digest.is_empty() && sample.taken_under(backend, digest, hints);
        let observed = self
            .samples
            .iter()
            .filter(comparable)
            .filter(|sample| sample.context >= context)
            .map(|sample| sample.bytes)
            .max();
        // Context 0 means the context is unknown, not empty: fall back to the assumed KV share.
        let kv = model
            .kv_cache_bytes_per_token_f16
            .filter(|_| context > 0)
            .and_then(|bytes| bytes.checked_mul(context))
            .and_then(|bytes| bytes.checked_mul(hints.num_parallel))
            .map(|bytes| bytes / 100 * hints.kv_cache_percent);
        let kv_or_fallback = kv.unwrap_or_else(|| model.size / 100 * UNKNOWN_KV_PERCENT_OF_FILE);
        let graph = model.size / 100 * GRAPH_MARGIN_PERCENT;
        // A measurement at a smaller context is still the best base when the KV shape is known:
        // Ollama's measured size plus exactly the extra KV cache the larger context needs.
        // Without KV metadata there is no safe way to scale it, so fall back to the estimate.
        let measured_plus_kv = if observed.is_some() {
            None
        } else {
            model.kv_cache_bytes_per_token_f16.and_then(|per_token| {
                let (smaller_context, bytes) = self
                    .samples
                    .iter()
                    .filter(comparable)
                    .filter(|sample| sample.context < context)
                    .map(|sample| (sample.context, sample.bytes))
                    .max()?;
                let extra = per_token
                    .checked_mul(context - smaller_context)?
                    .checked_mul(hints.num_parallel)?
                    / 100
                    * hints.kv_cache_percent;
                Some(bytes.saturating_add(extra))
            })
        };
        let estimate = observed.or(measured_plus_kv).unwrap_or_else(|| {
            model
                .size
                .saturating_add(kv_or_fallback)
                .saturating_add(graph)
        });
        json!({"required_available_bytes":estimate,"source":if observed.is_some() {"observed_same_digest_at_equal_or_larger_context"} else if measured_plus_kv.is_some() {"observed_smaller_context_plus_kv"} else if kv.is_some() {"model_file_plus_kv_plus_graph"} else {"model_file_plus_assumed_kv_plus_graph"},
            "exact":false,"kv_metadata_available":kv.is_some(),"history_samples":self.samples.len(),
            "ollama_num_parallel":hints.num_parallel,"kv_cache_percent_of_f16":hints.kv_cache_percent,
            "note":"capacity reservation estimate; OS telemetry and Ollama final admission remain authoritative"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persist(save: Option<FootprintSave>) {
        if let Some(save) = save {
            save.write();
        }
    }
    fn model() -> CatalogModel {
        serde_json::from_value(json!({"name":"m","digest":"a","size":1000,"capabilities":[],"advertised_context":32768,"resident":false,"benchmark":{},"policy_rank":{}})).unwrap()
    }

    #[test]
    fn zero_forecast_requires_exact_nonzero_context_and_known_matching_digest() {
        let history = FootprintHistory::default();
        let loaded = json!({"digest":"a","context_length":8192});
        let matching = history.requirement("cpu", &model(), 8192, Some(&loaded), true);
        assert_eq!(matching["required_available_bytes"], 0);
        assert_eq!(matching["source"], "matching_resident_context");
        for context in [0, 4096, 16384] {
            assert!(history.requirement("cpu", &model(), context, Some(&loaded), true)
                ["required_available_bytes"].as_u64().unwrap() > 0);
        }
        for digest in [None, Some(String::new()), Some("changed".into())] {
            let mut requested = model();
            requested.digest = digest;
            assert!(history.requirement("cpu", &requested, 8192, Some(&loaded), true)
                ["required_available_bytes"].as_u64().unwrap() > 0);
        }
        // Omitting the live entry is the conservative path for explicit unreported load controls.
        assert!(
            history.requirement("cpu", &model(), 8192, None, true)["required_available_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
    #[test]
    fn observed_footprints_never_cross_revisions_or_extrapolate_upward() {
        let mut history = FootprintHistory::default();
        persist(history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"verified","digest":"a","size":2000,"context_length":8192}),
        ));
        assert_eq!(
            history.requirement("cpu", &model(), 4096, None, true)["required_available_bytes"],
            2000
        );
        // No same-digest sample at this context: file + assumed KV (25%) + graph (10%).
        assert_eq!(
            history.requirement("cpu", &model(), 16384, None, true)["required_available_bytes"],
            1350
        );
        let mut changed = model();
        changed.digest = Some("b".into());
        assert_eq!(
            history.requirement("cpu", &changed, 4096, None, true)["required_available_bytes"],
            1350
        );
        assert_eq!(
            history.requirement("gpu", &model(), 4096, None, true)["required_available_bytes"],
            1350
        );
    }
    #[test]
    fn matching_residency_avoids_reserving_already_allocated_bytes() {
        let history = FootprintHistory::default();
        let current = json!({"digest":"a","size":2000,"context_length":8192});
        assert_eq!(
            history.requirement("cpu", &model(), 8192, Some(&current), true)["required_available_bytes"],
            0
        );
        assert_eq!(
            history.requirement("remote", &model(), 4096, None, false)["required_available_bytes"],
            0
        );
    }
    #[test]
    fn kv_scales_with_parallel_slots_and_quantized_cache() {
        let mut shaped = model();
        shaped.kv_cache_bytes_per_token_f16 = Some(10);
        let f16 = FootprintHistory::default();
        // 1000 file + 10 * 100 KV + 100 graph
        assert_eq!(
            f16.requirement("cpu", &shaped, 100, None, true)["required_available_bytes"],
            2100
        );
        let hints = RuntimeHints::from_lookup(|name| match name {
            "OLLAMA_NUM_PARALLEL" => Some("4".into()),
            "OLLAMA_KV_CACHE_TYPE" => Some("q8_0".into()),
            "OLLAMA_FLASH_ATTENTION" => Some("1".into()),
            _ => None,
        });
        let parallel_q8 = FootprintHistory::with_hints(hints);
        // KV: 10 * 100 * 4 slots * 50%
        assert_eq!(
            parallel_q8.requirement("cpu", &shaped, 100, None, true)["required_available_bytes"],
            3100
        );
        // Quantized cache without flash attention stays F16 in Ollama.
        let no_fa = RuntimeHints::from_lookup(|name| {
            (name == "OLLAMA_KV_CACHE_TYPE").then(|| "q4_0".into())
        });
        assert_eq!(no_fa.kv_cache_percent, 100);
    }

    #[test]
    fn each_backend_uses_its_own_parallelism_and_cache_settings() {
        let mut shaped = model();
        shaped.kv_cache_bytes_per_token_f16 = Some(10);
        let mut history = FootprintHistory::with_hints(RuntimeHints::default()).with_backend_hints(
            "cpu".into(),
            RuntimeHints {
                num_parallel: 4,
                kv_cache_percent: 50,
            },
        );
        // The default graph allowance is 1100; CPU KV is 100 * 10 * 4 * 50%.
        assert_eq!(
            history.requirement("gpu", &shaped, 100, None, true)["required_available_bytes"],
            2100
        );
        assert_eq!(
            history.requirement("cpu", &shaped, 100, None, true)["required_available_bytes"],
            3100
        );
        persist(history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"verified","digest":"a","size":5000,"context_length":100}),
        ));
        assert_eq!(
            history.requirement("cpu", &shaped, 100, None, true)["required_available_bytes"],
            5000
        );
        assert_eq!(history.samples.back().unwrap().num_parallel, 4);
        assert_eq!(history.samples.back().unwrap().kv_cache_percent, 50);
        // A restarted process may change parallelism. Measurements made under other
        // settings must not provide a too-small exact-fit estimate.
        history.set_backend_hints(
            "cpu".into(),
            RuntimeHints {
                num_parallel: 8,
                kv_cache_percent: 100,
            },
        );
        assert_eq!(
            history.requirement("cpu", &shaped, 100, None, true)["required_available_bytes"],
            9100
        );
        assert_eq!(
            history.requirement("gpu", &shaped, 100, None, true)["required_available_bytes"],
            2100
        );
    }

    #[test]
    fn a_smaller_measurement_grows_by_exactly_the_extra_kv_cache() {
        let mut shaped = model();
        shaped.kv_cache_bytes_per_token_f16 = Some(10);
        let mut history = FootprintHistory::default();
        persist(history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"verified","digest":"a","size":5000,"context_length":100}),
        ));
        let larger = history.requirement("cpu", &shaped, 300, None, true);
        // Measured 5000 at 100 tokens + 10 bytes * 200 more tokens.
        assert_eq!(larger["required_available_bytes"], 7000);
        assert_eq!(larger["source"], "observed_smaller_context_plus_kv");
    }

    #[test]
    fn resident_runners_are_learned_and_survive_a_restart() {
        let store = Store::in_memory().unwrap();
        let mut history = FootprintHistory::default().with_store(store.clone());
        let ps = json!({"models":[
            {"name":"m","digest":"a","size":4321,"context_length":8192},
            {"name":"no-digest","size":1,"context_length":1},
        ]});
        persist(history.observe_resident("cpu", &ps));
        // The same measurement again changes nothing and writes nothing.
        assert!(history.observe_resident("cpu", &ps).is_none());
        let restored = FootprintHistory::default().with_store(store);
        assert_eq!(
            restored.requirement("cpu", &model(), 4096, None, true)["required_available_bytes"],
            4321
        );
    }

    #[test]
    fn writes_from_two_processes_in_any_order_keep_the_larger_size() {
        let store = Store::in_memory().unwrap();
        let mut first = FootprintHistory::default().with_store(store.clone());
        let mut second = FootprintHistory::default().with_store(store.clone());
        let larger = first
            .observe_resident(
                "cpu",
                &json!({"models":[{"name":"m","digest":"a","size":4321,"context_length":8192}]}),
            )
            .unwrap();
        let smaller = second
            .observe_resident(
                "cpu",
                &json!({"models":[{"name":"m","digest":"a","size":1111,"context_length":8192}]}),
            )
            .unwrap();
        larger.write();
        smaller.write();
        let restored = FootprintHistory::default().with_store(store);
        assert_eq!(
            restored.requirement("cpu", &model(), 4096, None, true)["required_available_bytes"],
            4321
        );
    }

    #[test]
    fn a_restart_under_different_ollama_settings_ignores_old_measurements() {
        let store = Store::in_memory().unwrap();
        let mut history = FootprintHistory::default().with_store(store.clone());
        persist(history.observe_resident(
            "cpu",
            &json!({"models":[{"name":"m","digest":"a","size":4321,"context_length":8192}]}),
        ));
        // OLLAMA_NUM_PARALLEL went from 1 to 4: the one-slot size must not size a four-slot load.
        let four_slots = RuntimeHints {
            num_parallel: 4,
            kv_cache_percent: 100,
        };
        let restored = FootprintHistory::with_hints(four_slots).with_store(store);
        assert_ne!(
            restored.requirement("cpu", &model(), 4096, None, true)["source"],
            "observed_same_digest_at_equal_or_larger_context"
        );
    }

    #[test]
    fn history_is_bounded_and_rejects_unverified_samples() {
        let mut history = FootprintHistory::default();
        persist(history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"mismatch","size":1000,"context_length":1}),
        ));
        assert!(history.samples.is_empty());
        for context in 1..=100 {
            persist(history.observe(
                "cpu",
                Some("a"),
                &json!({"status":"verified","digest":"a","size":1000,"context_length":context}),
            ));
        }
        assert_eq!(history.samples.len(), 64);
    }
}
