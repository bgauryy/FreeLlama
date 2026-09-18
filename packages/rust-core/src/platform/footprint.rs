//! Bounded empirical runner footprints. No extrapolation across model revisions or contexts.
use super::CatalogModel;
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct FootprintHistory(VecDeque<Sample>);

struct Sample {
    backend: String,
    digest: String,
    context: u64,
    bytes: u64,
}

impl FootprintHistory {
    pub(super) fn observe(&mut self, backend: &str, digest: Option<&str>, observation: &Value) {
        let Some(digest) = digest.filter(|digest| !digest.is_empty()) else {
            return;
        };
        if observation["status"] != "verified" || observation["digest"].as_str() != Some(digest) {
            return;
        }
        let (Some(context), Some(bytes)) = (
            observation["context_length"].as_u64(),
            observation["size"].as_u64(),
        ) else {
            return;
        };
        if context == 0 || bytes == 0 {
            return;
        }
        let previous = self
            .0
            .iter()
            .find(|sample| {
                sample.backend == backend && sample.digest == digest && sample.context == context
            })
            .map_or(0, |sample| sample.bytes);
        self.0.retain(|sample| {
            !(sample.backend == backend && sample.digest == digest && sample.context == context)
        });
        self.0.push_back(Sample {
            backend: backend.to_owned(),
            digest: digest.to_owned(),
            context,
            bytes: bytes.max(previous),
        });
        while self.0.len() > 64 {
            self.0.pop_front();
        }
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
        let kv = model
            .kv_cache_bytes_per_token_f16
            .and_then(|bytes| bytes.checked_mul(context));
        let estimate = observed.unwrap_or_else(|| model.size.saturating_add(kv.unwrap_or(0)));
        json!({"required_available_bytes":estimate,"source":if observed.is_some() {"observed_same_digest_at_equal_or_larger_context"} else {"model_file_plus_known_f16_kv"},
            "exact":false,"kv_metadata_available":kv.is_some(),"history_samples":self.0.len(),
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
        assert_eq!(
            history.requirement("cpu", &model(), 16384, None, true)["required_available_bytes"],
            1000
        );
        let mut changed = model();
        changed.digest = Some("b".into());
        assert_eq!(
            history.requirement("cpu", &changed, 4096, None, true)["required_available_bytes"],
            1000
        );
        assert_eq!(
            history.requirement("gpu", &model(), 4096, None, true)["required_available_bytes"],
            1000
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
