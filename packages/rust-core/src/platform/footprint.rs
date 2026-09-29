//! Bounded empirical runner footprints. No extrapolation across model revisions or contexts.
use super::CatalogModel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::PathBuf};

/// Measured runner sizes, newest last, plus the settings that scale an estimate. With a file,
/// samples survive restarts, so estimates come from Ollama's own measurements from the start.
#[derive(Default)]
pub(super) struct FootprintHistory(VecDeque<Sample>, RuntimeHints, Option<PathBuf>);

/// Ollama settings that scale a runner's memory beyond file size + one F16 KV sequence.
///
/// Read from `FreeLlama`'s own environment, which usually matches an Ollama started from the same
/// shell or service definition; `doctor` reports where the Ollama process's values differ.
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
/// Measurements kept (one per backend, model revision and context size).
const HISTORY_LIMIT: usize = 64;

#[derive(Serialize, Deserialize)]
struct Sample {
    backend: String,
    digest: String,
    context: u64,
    bytes: u64,
}

impl FootprintHistory {
    pub(super) fn with_hints(hints: RuntimeHints) -> Self {
        Self(VecDeque::new(), hints, None)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    /// Load and keep saving samples at `path`. An unreadable or corrupt file starts empty: it is
    /// a cache of measurements, never a source of truth.
    pub(super) fn with_file(mut self, path: PathBuf) -> Self {
        if let Some(samples) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<Sample>>(&bytes).ok())
        {
            self.0 = samples
                .into_iter()
                .rev()
                .take(HISTORY_LIMIT)
                .rev()
                .collect();
        }
        self.2 = Some(path);
        self
    }

    fn save(&self) {
        let Some(path) = &self.2 else { return };
        // A few kilobytes at most, written only when a measurement changes.
        let Ok(bytes) = serde_json::to_vec(&self.0) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(&temporary, path);
        }
    }

    /// Learn from every runner Ollama reports as resident (`/api/ps`), including ones loaded by
    /// raw clients, so `FreeLlama`'s estimates converge on Ollama's measured sizes.
    pub(super) fn observe_resident(&mut self, backend: &str, ps: &Value) {
        let mut changed = false;
        for entry in ps["models"].as_array().into_iter().flatten() {
            let digest = entry["digest"].as_str();
            let mut observation = entry.clone();
            observation["status"] = json!("verified");
            changed |= self.record(backend, digest, &observation);
        }
        if changed {
            self.save();
        }
    }

    pub(super) fn observe(&mut self, backend: &str, digest: Option<&str>, observation: &Value) {
        if self.record(backend, digest, observation) {
            self.save();
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
        let previous = self
            .0
            .iter()
            .find(|sample| {
                sample.backend == backend && sample.digest == digest && sample.context == context
            })
            .map_or(0, |sample| sample.bytes);
        if previous >= bytes {
            return false;
        }
        self.0.retain(|sample| {
            !(sample.backend == backend && sample.digest == digest && sample.context == context)
        });
        self.0.push_back(Sample {
            backend: backend.to_owned(),
            digest: digest.to_owned(),
            context,
            bytes: bytes.max(previous),
        });
        while self.0.len() > HISTORY_LIMIT {
            self.0.pop_front();
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
                .is_some_and(|loaded| loaded >= context)
                && model
                    .digest
                    .as_ref()
                    .is_some_and(|digest| entry["digest"].as_str() == Some(digest.as_str()))
        });
        if warm {
            return json!({"required_available_bytes":0,"source":"matching_resident_context","exact":false,
                "note":"resident allocation already reflected in host telemetry; runner growth remains possible"});
        }
        let observed = self
            .0
            .iter()
            .filter(|sample| {
                sample.backend == backend
                    && model.digest.as_deref() == Some(sample.digest.as_str())
                    && sample.context >= context
            })
            .map(|sample| sample.bytes)
            .max();
        let hints = self.1;
        let kv = model
            .kv_cache_bytes_per_token_f16
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
                    .0
                    .iter()
                    .filter(|sample| {
                        sample.backend == backend
                            && model.digest.as_deref() == Some(sample.digest.as_str())
                            && sample.context < context
                    })
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
            "exact":false,"kv_metadata_available":kv.is_some(),"history_samples":self.0.len(),
            "ollama_num_parallel":hints.num_parallel,"kv_cache_percent_of_f16":hints.kv_cache_percent,
            "note":"capacity reservation estimate; OS telemetry and Ollama final admission remain authoritative"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model() -> CatalogModel {
        serde_json::from_value(json!({"name":"m","digest":"a","size":1000,"capabilities":[],"advertised_context":32768,"resident":false,"benchmark":{},"policy_rank":{}})).unwrap()
    }
    #[test]
    fn observed_footprints_never_cross_revisions_or_extrapolate_upward() {
        let mut history = FootprintHistory::default();
        history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"verified","digest":"a","size":2000,"context_length":8192}),
        );
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
            history.requirement("cpu", &model(), 4096, Some(&current), true)["required_available_bytes"],
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
    fn a_smaller_measurement_grows_by_exactly_the_extra_kv_cache() {
        let mut shaped = model();
        shaped.kv_cache_bytes_per_token_f16 = Some(10);
        let mut history = FootprintHistory::default();
        history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"verified","digest":"a","size":5000,"context_length":100}),
        );
        let larger = history.requirement("cpu", &shaped, 300, None, true);
        // Measured 5000 at 100 tokens + 10 bytes * 200 more tokens.
        assert_eq!(larger["required_available_bytes"], 7000);
        assert_eq!(larger["source"], "observed_smaller_context_plus_kv");
    }

    #[test]
    fn resident_runners_are_learned_and_survive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("footprints.json");
        let mut history = FootprintHistory::default().with_file(path.clone());
        let ps = json!({"models":[
            {"name":"m","digest":"a","size":4321,"context_length":8192},
            {"name":"no-digest","size":1,"context_length":1},
        ]});
        history.observe_resident("cpu", &ps);
        let saved = std::fs::metadata(&path).unwrap().modified().unwrap();
        // The same measurement again changes nothing and is not rewritten.
        history.observe_resident("cpu", &ps);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), saved);
        let restored = FootprintHistory::default().with_file(path);
        assert_eq!(
            restored.requirement("cpu", &model(), 4096, None, true)["required_available_bytes"],
            4321
        );
    }

    #[test]
    fn history_is_bounded_and_rejects_unverified_samples() {
        let mut history = FootprintHistory::default();
        history.observe(
            "cpu",
            Some("a"),
            &json!({"status":"mismatch","size":1000,"context_length":1}),
        );
        assert!(history.0.is_empty());
        for context in 1..=100 {
            history.observe(
                "cpu",
                Some("a"),
                &json!({"status":"verified","digest":"a","size":1000,"context_length":context}),
            );
        }
        assert_eq!(history.0.len(), 64);
    }
}
