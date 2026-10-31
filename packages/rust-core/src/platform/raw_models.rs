//! Raw passthrough traffic in the platform's residency accounting. Clients such as a decision
//! or embedding caller talk to Ollama's own API through `serve`; without this, the eviction
//! planner would see their models as unused and unload them first, and their cold loads would
//! skip the memory budget.

use std::time::Duration;

use super::{Backend, PlatformState, audit, budget, catalog, execution, footprint};
use crate::proxy::{HookFuture, RawModelHooks};

/// How long a raw load may spend making room before it is forwarded anyway.
const ROOM_WAIT: Duration = Duration::from_secs(10);

pub(super) struct RawModels(pub(super) PlatformState);

impl RawModelHooks for RawModels {
    fn prepare<'a>(&'a self, model: &'a str, context: Option<u64>) -> HookFuture<'a> {
        Box::pin(make_room(&self.0, model, context))
    }

    fn begin(&self, model: &str) -> Box<dyn Send> {
        Box::new(self.0.activity.begin(Backend::Gpu.as_str(), model))
    }

    fn served(&self, model: &str, _loaded: bool) {
        self.0.activity.completed(Backend::Gpu.as_str(), model, 0);
        super::preload::served(&self.0, Backend::Gpu.as_str(), model);
    }

    /// The response streamed through untouched; its final counts, read on the way, become a
    /// usage-ledger row, and a load it caused is audited with the time Ollama reported for it.
    fn finished(&self, finished: crate::proxy::RawFinished<'_>) {
        let state = &self.0;
        let backend = finished.backend;
        let ollama = backend == Backend::Gpu.as_str();
        let load_ms = finished
            .usage
            .load_duration_ns
            .filter(|_| ollama)
            .map(|ns| ns / 1_000_000);
        if let Some(load_ms) = load_ms {
            state.activity.learn_load(backend, finished.model, load_ms);
        }
        if ollama && finished.loaded {
            state.audit.record(&audit::Entry {
                backend,
                model: finished.model,
                event: "load",
                trigger: "raw_request",
                duration_ms: load_ms.filter(|ms| super::residency::was_a_load(*ms)),
                bytes: None,
            });
        }
        let output_tokens = finished.usage.output_tokens.unwrap_or(0);
        state.telemetry.record_task(super::telemetry::TaskRecord {
            ts: super::telemetry::now_seconds(),
            model: finished.model.to_owned(),
            backend: backend.to_owned(),
            task: "raw".into(),
            priority: finished.path.to_owned(),
            outcome: "ok".into(),
            status: 200,
            prompt_tokens: finished.usage.prompt_tokens.unwrap_or(0),
            output_tokens,
            duration_ms: finished.elapsed_ms,
            queue_wait_ms: 0,
            load_ms: load_ms.unwrap_or(0),
            output_tokens_per_second: None,
            cached_prompt_tokens: finished.usage.cached_prompt_tokens.unwrap_or(0),
            client: finished.caller.map(str::to_owned),
        });
    }

    fn unloaded(&self, model: &str) {
        self.0.audit.record(&audit::Entry {
            backend: Backend::Gpu.as_str(),
            model,
            event: "unload",
            trigger: "caller_unload",
            duration_ms: None,
            bytes: None,
        });
    }
}

/// Size a raw load and run the same room-making steps as a managed load (GPU fit, memory budget,
/// host pressure). The whole preparation is bounded by [`ROOM_WAIT`]: it runs while the request
/// holds the backend's transition lock, so a slow backend must not stall managed work behind it.
/// A model the primary backend does not list is forwarded unchanged: Ollama reports the error.
async fn make_room(state: &PlatformState, model: &str, context: Option<u64>) {
    if !state.tunables().evict_idle_models {
        return;
    }
    let deadline = tokio::time::Instant::now() + ROOM_WAIT;
    let prepared = tokio::time::timeout_at(deadline, async {
        let target = execution::primary_target(state);
        let model = catalog::primary_model(state, model).await?;
        if !may_need_room(state, &target, &model).await {
            return Some(());
        }
        // Without an explicit `num_ctx`, Ollama loads at its own default context; size for that.
        let context = match context {
            Some(context) => context,
            None => super::monitor::ollama_default_context(state, Some(&model), Backend::Gpu)
                .await
                .map_or(0, |(tokens, _)| tokens),
        };
        execution::make_room(state, &target, &model, context, deadline).await;
        Some(())
    })
    .await;
    if !matches!(prepared, Ok(Some(()))) {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| {
            eprintln!(
                "freellama: a raw load was forwarded without making room (model not listed on the \
                 primary backend, or sizing took longer than {}s)",
                ROOM_WAIT.as_secs()
            );
        });
    }
}

/// A cheap check before sizing a raw load properly: is room plausibly needed? Yes while the host
/// holds for pressure; on a discrete GPU when the model file alone exceeds free VRAM; where models
/// share host memory, when the file plus the usual margins would leave less than the host reserve
/// or pass the memory budget.
pub(super) async fn may_need_room(
    state: &PlatformState,
    target: &execution::ExecutionTarget,
    model: &super::CatalogModel,
) -> bool {
    let snapshot = state.resources.snapshot().await;
    if snapshot.holding {
        return true;
    }
    let estimate = footprint::file_only_estimate(model.size);
    if !execution::host_memory_relevant(&target.upstream, target.placement) {
        return snapshot
            .observation
            .gpu_memory_free_bytes
            .is_some_and(|free| estimate > free);
    }
    let assessment = snapshot.assess_capacity(estimate, false);
    if !assessment.admissible || assessment.memory_advisory.is_some() {
        return true;
    }
    let Some(budget) = state
        .tunables()
        .model_memory_budget
        .bytes(super::host_total_memory_bytes())
    else {
        return false;
    };
    let Ok(ps) = super::get_json(&state.client, &target.upstream, "/api/ps").await else {
        return false;
    };
    budget::overage(budget, budget::resident_bytes(&ps, &model.name), estimate) > 0
}
