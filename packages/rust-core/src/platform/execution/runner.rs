//! Conservative runner reuse from Ollama's observable identity and effective context.

use serde_json::{Value, json};

use super::super::{CatalogModel, RouteDecision};

/// Where a request needs its runner's weights, judged from `/api/ps` `size_vram`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementRule {
    /// No placement requirement: raw passthrough, or evidence below `observed`.
    Any,
    /// `num_gpu: 0` was requested: every byte must be in host memory.
    CpuOnly,
    /// A GPU-assigned runner whose placement evidence must be observed: some bytes in VRAM.
    GpuResident,
}

impl PlacementRule {
    /// The one rule for a runner on `backend`. Raw passthrough calls this with `observed: false`.
    pub(crate) fn for_backend(
        backend: super::super::Backend,
        placement_observed: bool,
        cpu_only_requested: bool,
    ) -> Self {
        if cpu_only_requested {
            Self::CpuOnly
        } else if placement_observed && backend == super::super::Backend::Gpu {
            Self::GpuResident
        } else {
            Self::Any
        }
    }

    pub(crate) fn satisfied_by(self, entry: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::CpuOnly => entry["size_vram"].as_u64() == Some(0),
            Self::GpuResident => entry["size_vram"].as_u64().is_some_and(|vram| vram > 0),
        }
    }
}

/// `/api/ps` cannot prove every load option. Explicit overrides of unreported runner controls
/// therefore require the exclusive transition path even if digest and context happen to match.
///
/// Degraded identity: a matching digest without `context_length` is compatible only when the
/// request sends no `num_ctx` (Ollama's default context applies, so there is no explicit context
/// to contradict). A missing digest is never compatible: the runner could be any build of the tag.
#[derive(Clone)]
pub(crate) struct RunnerExpectation {
    digest: Option<String>,
    context_tokens: Option<u64>,
    /// The request itself sends `num_ctx`, so an unreported runner context cannot be trusted.
    context_explicit: bool,
    unverified_options: Vec<String>,
    placement: PlacementRule,
}

impl RunnerExpectation {
    pub(crate) fn new(model: &CatalogModel, route: &RouteDecision) -> Self {
        let context_tokens = route.options["num_ctx"]
            .as_u64()
            .filter(|tokens| *tokens > 0);
        Self {
            digest: model.digest.clone().filter(|digest| !digest.is_empty()),
            context_tokens,
            context_explicit: context_tokens.is_some(),
            unverified_options: Vec::new(),
            placement: PlacementRule::Any,
        }
        .with_runtime_options(&route.options)
    }

    /// Include controls injected by the managed backend after route selection. `options` is the
    /// request as sent: an absent `num_ctx` leaves the context to Ollama's default.
    pub(crate) fn with_runtime_options(mut self, options: &Value) -> Self {
        self.unverified_options = [
            "num_batch",
            "num_thread",
            "main_gpu",
            "numa",
            "low_vram",
            "f16_kv",
            "logits_all",
            "vocab_only",
            "use_mmap",
            "use_mlock",
            "embedding_only",
        ]
        .into_iter()
        .filter(|key| options.get(*key).is_some())
        .map(str::to_owned)
        .collect();
        self.context_explicit = options.get("num_ctx").is_some_and(|value| !value.is_null());
        if options["num_gpu"].as_i64() == Some(0) {
            self.placement = PlacementRule::CpuOnly;
        }
        self
    }

    /// Require observed VRAM residency from a GPU-assigned runner (placement evidence `observed`).
    pub(crate) fn requiring_gpu_placement(mut self, backend: super::super::Backend) -> Self {
        if self.placement != PlacementRule::CpuOnly {
            self.placement = PlacementRule::for_backend(backend, true, false);
        }
        self
    }

    pub(crate) fn has_unverified_options(&self) -> bool {
        !self.unverified_options.is_empty()
    }

    pub(crate) fn matches(&self, entry: &Value) -> bool {
        self.identity_matches(entry)
            && !self.has_unverified_options()
            && self.placement.satisfied_by(entry)
    }

    /// Whether any same-name runner in `ps` is compatible: Ollama can hold one name at several
    /// contexts, and sharing needs only one of them to fit.
    pub(crate) fn matches_in(&self, ps: &Value, model: &str) -> bool {
        crate::ollama_http::ps_entries(ps, model).any(|entry| self.matches(entry))
    }

    /// The runner this request would use: a compatible one, else the digest match, else the first.
    fn entry_in<'a>(&self, ps: &'a Value, model: &str) -> Option<&'a Value> {
        crate::ollama_http::ps_entries(ps, model)
            .find(|entry| self.matches(entry))
            .or_else(|| {
                crate::ollama_http::find_ps_entry_by_digest(ps, model, self.digest.as_deref())
            })
    }

    /// Identity matching is also used after forwarding. It does not claim that `/api/ps`
    /// reports CPU thread settings, batch sizes, or physical file-page residency.
    pub(crate) fn identity_matches(&self, entry: &Value) -> bool {
        self.digest_matches(entry) && self.context_matches(entry)
    }

    fn digest_matches(&self, entry: &Value) -> bool {
        self.digest
            .as_deref()
            .is_some_and(|digest| entry["digest"].as_str() == Some(digest))
    }

    fn context_matches(&self, entry: &Value) -> bool {
        match entry["context_length"].as_u64() {
            Some(observed) => self.context_tokens == Some(observed),
            None => !self.context_explicit,
        }
    }

    /// Why a present runner is or is not shareable, for the receipt; `None` for a plain match.
    fn reason(&self, entry: &Value) -> Option<&'static str> {
        let unobserved_digest = self.digest.is_none() || entry["digest"].as_str().is_none();
        if unobserved_digest {
            Some("resident_identity_unobserved")
        } else if !self.digest_matches(entry) {
            Some("resident_digest_changed")
        } else if self.has_unverified_options() {
            Some("unverified_load_options")
        } else if !self.context_matches(entry) {
            Some(if entry["context_length"].is_null() {
                "resident_context_unobserved"
            } else {
                "resident_context_mismatch"
            })
        } else if !self.placement.satisfied_by(entry) {
            Some("resident_placement_mismatch")
        } else if entry["context_length"].is_null() {
            Some("resident_context_unobserved_default_context_assumed")
        } else {
            None
        }
    }

    pub(crate) fn receipt(&self, ps: Option<&Value>, model: &str) -> Value {
        let known = ps.and_then(|ps| ps["models"].as_array()).is_some();
        let entry = ps.and_then(|ps| self.entry_in(ps, model));
        let compatible = entry.is_some_and(|entry| self.matches(entry));
        json!({
            "status": if compatible { "compatible" } else if entry.is_some() { "incompatible_or_unverified" } else if known { "absent" } else { "unavailable" },
            "compatible": compatible,
            "reason": entry.and_then(|entry| self.reason(entry)),
            "expected_digest": self.digest,
            "expected_context_tokens": self.context_tokens,
            "context_explicit": self.context_explicit,
            "observed_digest": entry.and_then(|entry| entry["digest"].as_str()),
            "observed_context_tokens": entry.and_then(|entry| entry["context_length"].as_u64()),
            "unverified_load_options": self.unverified_options,
            "placement_rule": self.placement_label(),
            "requires_cpu_only": self.placement == PlacementRule::CpuOnly,
            "source": "ollama_api_ps_before_execution",
            "scope": "observable_digest_and_effective_context",
        })
    }

    fn placement_label(&self) -> &'static str {
        match self.placement {
            PlacementRule::Any => "any",
            PlacementRule::CpuOnly => "cpu_only",
            PlacementRule::GpuResident => "gpu_resident",
        }
    }
}

/// A proven transition is an absent runner followed by the requested observable identity.
/// Ollama's timer includes upstream scheduling/setup; file-page warmth is not observable here.
pub(crate) fn completion_receipt(before: &Value, after: &Value, duration_ns: Option<u64>) -> Value {
    let identity_verified = before["expected_digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .is_some_and(|digest| after["digest"].as_str() == Some(digest))
        && before["expected_context_tokens"]
            .as_u64()
            .filter(|tokens| *tokens > 0)
            .is_some_and(|tokens| after["context_length"].as_u64() == Some(tokens));
    let transition = before["status"] == "absent" && identity_verified && after["resident"] == true;
    json!({
        "transition_observed": transition,
        "identity_verified": identity_verified,
        "learning_eligible": transition && duration_ns.is_some_and(|duration| duration > 0),
        "reported_duration_ns": duration_ns,
        "duration_scope": "ollama_request_to_runner_ready_including_upstream_scheduling",
        "storage_cache_state": "unknown",
        "cold_disk_verified": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{RouteInput, SessionAffinity, select_route};

    fn expectation() -> RunnerExpectation {
        let model: CatalogModel = serde_json::from_value(json!({
            "name":"test:latest", "digest":"revision-a", "size":1,
            "capabilities":["completion"], "advertised_context":32768,
            "resident":true, "benchmark":{}, "policy_rank":{}
        }))
        .unwrap();
        let input: RouteInput = serde_json::from_value(json!({
            "task":"completion", "context_tokens":4096
        }))
        .unwrap();
        let route = select_route(
            &input,
            std::slice::from_ref(&model),
            &SessionAffinity::default(),
        )
        .unwrap();
        RunnerExpectation::new(&model, &route)
    }

    #[test]
    fn duration_alone_never_proves_a_load_or_disk_cache_state() {
        let expected = expectation();
        let after = json!({"resident":true,"digest":"revision-a","context_length":4096});
        let absent = expected.receipt(Some(&json!({"models":[]})), "test:latest");
        let transition = completion_receipt(&absent, &after, Some(50_000_000));
        assert_eq!(transition["learning_eligible"], true);
        assert_eq!(transition["reported_duration_ns"], 50_000_000);
        assert_eq!(transition["cold_disk_verified"], false);
        assert_eq!(transition["storage_cache_state"], "unknown");
        let present = expected.receipt(
            Some(&json!({"models":[{
                "name":"test:latest","digest":"revision-a","context_length":4096
            }]})),
            "test:latest",
        );
        for before in [present, expected.receipt(None, "test:latest")] {
            let queued = completion_receipt(&before, &after, Some(30_000_000_000));
            assert_eq!(queued["learning_eligible"], false);
        }
        let wrong = json!({"resident":true,"digest":"revision-a","context_length":8192});
        assert_eq!(
            completion_receipt(&absent, &wrong, Some(1))["learning_eligible"],
            false
        );
        assert_eq!(
            completion_receipt(&absent, &after, None)["learning_eligible"],
            false
        );
    }

    #[test]
    fn a_name_match_cannot_release_a_transition_for_the_old_runner() {
        let expected = expectation();
        for entry in [
            json!({"name":"test:latest"}),
            json!({"name":"test:latest","digest":"revision-b","context_length":4096}),
            json!({"name":"test:latest","digest":"revision-a","context_length":2048}),
            json!({"name":"test:latest","digest":"revision-a","context_length":8192}),
        ] {
            assert!(!expected.matches(&entry));
        }
        assert!(expected.matches(&json!({"digest":"revision-a","context_length":4096})));
    }

    #[test]
    fn an_unreported_context_is_shared_only_when_the_request_leaves_context_to_ollama() {
        let entry = json!({"name":"test:latest","digest":"revision-a"});
        // The route sized 4096 and sends it: an unreported runner context cannot be trusted.
        let explicit = expectation().with_runtime_options(&json!({"num_ctx":4096}));
        assert!(!explicit.matches(&entry));
        let ps = json!({"models":[entry.clone()]});
        assert_eq!(
            explicit.receipt(Some(&ps), "test:latest")["reason"],
            "resident_context_unobserved"
        );
        // Ollama's default context applies when the request sends none.
        let automatic = expectation().with_runtime_options(&json!({"num_predict":8}));
        assert!(automatic.matches(&entry));
        let receipt = automatic.receipt(Some(&ps), "test:latest");
        assert_eq!(receipt["compatible"], true);
        assert_eq!(
            receipt["reason"],
            "resident_context_unobserved_default_context_assumed"
        );
        // A reported context still has to agree, and unverified load options still exclude.
        let reported = json!({"digest":"revision-a","context_length":2048});
        assert!(!automatic.matches(&reported));
        let batched = expectation().with_runtime_options(&json!({"num_batch":8}));
        assert!(!batched.matches(&entry));
        assert_eq!(
            batched.receipt(Some(&ps), "test:latest")["reason"],
            "unverified_load_options"
        );
    }

    #[test]
    fn a_missing_digest_stays_exclusive_and_the_receipt_says_why() {
        let automatic = expectation().with_runtime_options(&json!({}));
        for entry in [
            json!({"name":"test:latest","context_length":4096}),
            json!({"name":"test:latest"}),
        ] {
            assert!(!automatic.matches(&entry));
            let receipt = automatic.receipt(Some(&json!({"models":[entry]})), "test:latest");
            assert_eq!(receipt["status"], "incompatible_or_unverified");
            assert_eq!(receipt["reason"], "resident_identity_unobserved");
        }
        let changed = json!({"models":[{"name":"test:latest","digest":"revision-b"}]});
        assert_eq!(
            automatic.receipt(Some(&changed), "test:latest")["reason"],
            "resident_digest_changed"
        );
        // A catalog model with no digest cannot be matched to any runner.
        let mut unknown = expectation();
        unknown.digest = None;
        assert!(!unknown.matches(&json!({"digest":"revision-a","context_length":4096})));
    }

    #[test]
    fn placement_is_checked_for_gpu_runners_when_evidence_is_required() {
        use crate::platform::Backend;
        let cpu_resident =
            json!({"digest":"revision-a","context_length":4096,"size":10,"size_vram":0});
        let gpu_resident =
            json!({"digest":"revision-a","context_length":4096,"size":10,"size_vram":10});
        let unreported = json!({"digest":"revision-a","context_length":4096});
        let relaxed = expectation();
        assert!(
            relaxed.matches(&cpu_resident),
            "no evidence requirement, no placement rule"
        );
        let strict = expectation().requiring_gpu_placement(Backend::Gpu);
        assert!(strict.matches(&gpu_resident));
        assert!(!strict.matches(&cpu_resident));
        assert!(
            !strict.matches(&unreported),
            "unreported VRAM is not observed placement"
        );
        let ps = json!({"models":[{"name":"test:latest","digest":"revision-a","context_length":4096,"size":10,"size_vram":0}]});
        assert_eq!(
            strict.receipt(Some(&ps), "test:latest")["reason"],
            "resident_placement_mismatch"
        );
        // The CPU backend never gets a GPU requirement, and an explicit num_gpu:0 stays CPU-only.
        assert!(
            expectation()
                .requiring_gpu_placement(Backend::Cpu)
                .matches(&cpu_resident)
        );
        let cpu_only = expectation()
            .with_runtime_options(&json!({"num_ctx":4096,"num_gpu":0}))
            .requiring_gpu_placement(Backend::Gpu);
        assert!(cpu_only.matches(&cpu_resident));
        assert!(!cpu_only.matches(&gpu_resident));
    }

    #[test]
    fn sharing_looks_at_every_same_name_runner_not_the_first() {
        let expected = expectation();
        let ps = json!({"models":[
            {"name":"test:latest","digest":"revision-a","context_length":2048},
            {"name":"test:latest","digest":"revision-a","context_length":4096},
        ]});
        assert!(expected.matches_in(&ps, "test:latest"));
        let receipt = expected.receipt(Some(&ps), "test:latest");
        assert_eq!(receipt["compatible"], true);
        assert_eq!(receipt["observed_context_tokens"], 4096);
        let none = json!({"models":[
            {"name":"test:latest","digest":"revision-b","context_length":4096},
            {"name":"test:latest","digest":"revision-a","context_length":2048},
        ]});
        assert!(!expected.matches_in(&none, "test:latest"));
        // The digest-matching runner is the one reported when nothing is compatible.
        assert_eq!(
            expected.receipt(Some(&none), "test:latest")["observed_digest"],
            "revision-a"
        );
    }

    #[test]
    fn unavailable_state_is_not_a_proven_load_and_unknown_controls_are_not_reused() {
        let mut expected = expectation();
        assert_eq!(
            expected.receipt(None, "test:latest")["status"],
            "unavailable"
        );
        assert_eq!(
            expected.receipt(Some(&json!({"models":[]})), "test:latest")["status"],
            "absent"
        );
        expected.unverified_options.push("num_batch".into());
        let entry = json!({"digest":"revision-a","context_length":4096});
        assert!(expected.identity_matches(&entry));
        assert!(!expected.matches(&entry));
    }
}
