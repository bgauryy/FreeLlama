//! Read-only monitoring endpoints: Prometheus metrics, a live status view, usage totals, and the
//! effective runtime configuration (with a reload trigger).

use axum::{
    Json,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::telemetry::Gauge;
use super::{CatalogModel, PlatformState, get_json, host_has_unified_memory, ollama_env, runtime};

/// Ollama's own default `num_ctx` for `model` on `placement`, when it can be known, with the
/// source it came from. Order follows Ollama's resolution in `server/routes.go`: a Modelfile
/// `num_ctx` wins, then `OLLAMA_CONTEXT_LENGTH`, then the VRAM tier chosen at server start.
///
/// The VRAM tier is only inferred on the primary backend with discrete-GPU telemetry. On
/// unified memory Ollama sizes the tier from Metal's working-set limit, which `FreeLlama` does
/// not read, and a CPU-only process may still see GPUs, so both stay unknown unless configured.
pub(super) async fn ollama_default_context(
    state: &PlatformState,
    model: Option<&CatalogModel>,
    placement: &str,
) -> Option<(u64, &'static str)> {
    if let Some(value) = model.and_then(|model| model.modelfile_num_ctx) {
        return Some((value, "modelfile"));
    }
    if let Some(value) = state.tunables().ollama_default_context {
        return Some((value, "config"));
    }
    if placement == "cpu" {
        return None;
    }
    if let Some(value) = state.ollama.context_length() {
        return Some((value, "ollama_context_length"));
    }
    if host_has_unified_memory() {
        return None;
    }
    let observation = state.resources.snapshot().await.observation;
    let total = observation.gpu_memory_total_bytes?;
    Some((
        ollama_env::vram_tier_default_context(
            total.saturating_sub(state.ollama.gpu_overhead_bytes()),
        ),
        "vram_tier",
    ))
}

fn backend_views(state: &PlatformState) -> Vec<(&'static str, String, &super::AdmissionPool)> {
    let mut views = vec![("gpu", state.upstream.clone(), &state.gpu_admission)];
    if let Some(upstream) = &state.cpu_upstream {
        views.push(("cpu", upstream.clone(), &state.cpu_admission));
    }
    views
}

fn adaptive_enabled(mode: runtime::AdaptiveMode, backend: &str) -> bool {
    match mode {
        runtime::AdaptiveMode::Off => false,
        runtime::AdaptiveMode::Cpu => backend == "cpu",
        runtime::AdaptiveMode::All => true,
    }
}

pub(super) fn adaptive_applies(state: &PlatformState, backend: &str) -> bool {
    adaptive_enabled(state.tunables().adaptive_concurrency, backend)
}

/// Loaded runners per backend from `/api/ps`, or the error that prevented reading them.
async fn loaded_models(state: &PlatformState) -> Vec<Value> {
    let mut loaded = Vec::new();
    for (backend, upstream, _) in backend_views(state) {
        match get_json(&state.client, &upstream, "/api/ps").await {
            Ok(ps) => {
                for entry in ps["models"].as_array().into_iter().flatten() {
                    let size = entry["size"].as_u64().unwrap_or(0);
                    let vram = entry["size_vram"].as_u64().unwrap_or(0);
                    loaded.push(json!({
                        "backend": backend,
                        "name": entry.get("name").or_else(|| entry.get("model")),
                        "size_bytes": size,
                        "size_vram_bytes": vram,
                        "gpu_percent": if size > 0 { vram.saturating_mul(100) / size } else { 0 },
                        "context_length": entry.get("context_length"),
                        "expires_at": entry.get("expires_at"),
                        "pinned": state.tunables().pinned_models.contains(
                            entry.get("name").or_else(|| entry.get("model"))
                                .and_then(Value::as_str).unwrap_or_default()
                        ),
                    }));
                }
            }
            Err(error) => loaded.push(json!({"backend": backend, "error": error.body.error})),
        }
    }
    loaded
}

/// One compact view of what the machine and `FreeLlama` are doing right now.
pub(super) async fn status(State(state): State<PlatformState>) -> Json<Value> {
    let snapshot = state.resources.snapshot().await;
    let tunables = state.tunables();
    let breakers = state.breakers.receipt();
    let backends = backend_views(&state)
        .into_iter()
        .map(|(backend, upstream, pool)| {
            (
                backend.to_owned(),
                json!({
                    "upstream": upstream,
                    "admission": pool.receipt(),
                    "adaptive": state.adaptive_for(backend).receipt(
                        pool,
                        adaptive_enabled(tunables.adaptive_concurrency, backend),
                    ),
                    "circuit": breakers.get(backend).cloned()
                        .unwrap_or_else(|| json!({"state": "closed"})),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let default_context = ollama_default_context(&state, None, "gpu").await;
    let observation = &snapshot.observation;
    Json(json!({
        "status": if snapshot.holding { "holding" } else { "ok" },
        "backends": backends,
        "raw_proxy": state.raw_admission.receipt(),
        "loaded_models": loaded_models(&state).await,
        "host": {
            "status": snapshot.status,
            "holding": snapshot.holding,
            "reasons": snapshot.reasons,
            "total_memory_bytes": observation.total_memory_bytes,
            "available_memory_bytes": observation.available_memory_bytes,
            "effective_available_bytes": snapshot.effective_available_bytes,
            "reserved_bytes": snapshot.reserved_bytes,
            "memory_pressure": observation.memory_pressure,
            "memory_psi_some_avg10": observation.memory_psi_some_avg10,
            "cgroup_memory_limit_bytes": observation.cgroup_memory_limit_bytes,
            "load_average_one_minute": observation.load_average_one_minute,
            "logical_cpus": observation.logical_cpus,
            "thermal_throttled": observation.thermal_throttled,
            "gpu_memory_total_bytes": observation.gpu_memory_total_bytes,
            "gpu_memory_free_bytes": observation.gpu_memory_free_bytes,
            "gpu_telemetry_source": observation.gpu_telemetry_source,
            "sample_age_ms": snapshot.sample_age_ms,
        },
        "ollama": {
            "config": state.ollama.receipt(),
            "default_context": default_context.map(|(tokens, source)| json!({"tokens": tokens, "source": source})),
            "context_mode": tunables.context_mode,
        },
        "usage_today": state.telemetry.usage(1)["totals"].clone(),
        "usage_ledger": state.telemetry.ledger_receipt(),
        "runtime_config": {
            "file": state.runtime.file(),
            "reload": state.runtime.receipt()["reload"].clone(),
        },
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct UsageQuery {
    days: Option<u32>,
}

pub(super) async fn usage(
    State(state): State<PlatformState>,
    Query(query): Query<UsageQuery>,
) -> Json<Value> {
    Json(state.telemetry.usage(query.days.unwrap_or(7).clamp(1, 400)))
}

pub(super) async fn config(State(state): State<PlatformState>) -> Json<Value> {
    let mut receipt = state.runtime.receipt();
    receipt["ollama"] = state.ollama.receipt();
    Json(receipt)
}

pub(super) async fn reload_config(State(state): State<PlatformState>) -> Response {
    match state.runtime.reload(true) {
        Ok(changed) => {
            state.apply_tunables();
            let mut receipt = state.runtime.receipt();
            receipt["changed"] = json!(changed.is_some());
            Json(receipt).into_response()
        }
        Err(error) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": error, "kept": state.runtime.receipt()["settings"].clone()})),
        )
            .into_response(),
    }
}

#[allow(clippy::cast_precision_loss)]
fn float(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::too_many_lines)] // One flat list of gauge families is easier to audit than helpers.
pub(super) async fn metrics(State(state): State<PlatformState>) -> Response {
    let snapshot = state.resources.snapshot().await;
    let tunables = state.tunables();
    let mut gauges = Vec::new();
    let views = backend_views(&state);
    let receipts: Vec<(&str, Value)> = views
        .iter()
        .map(|(backend, _, pool)| (*backend, pool.receipt()))
        .collect();
    let per_backend = |gauges: &mut Vec<Gauge>,
                       name: &'static str,
                       help: &'static str,
                       field: &str,
                       counter: bool| {
        for (backend, receipt) in &receipts {
            let gauge = Gauge::new(name, help, receipt[field].as_f64().unwrap_or(0.0))
                .label("backend", *backend);
            gauges.push(if counter { gauge.counter() } else { gauge });
        }
    };
    per_backend(
        &mut gauges,
        "freellama_admission_limit_units",
        "Current admission capacity in cost units (adaptive).",
        "slots_total",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_admission_ceiling_units",
        "Configured maximum admission capacity in cost units.",
        "slots_ceiling",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_admission_active_units",
        "Cost units held by running tasks.",
        "active_units",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_tasks_in_flight",
        "Managed tasks currently admitted.",
        "in_flight",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_queue_depth",
        "Managed tasks waiting for admission.",
        "queue_depth",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_queue_limit",
        "Maximum managed tasks allowed to wait.",
        "queue_limit",
        false,
    );
    per_backend(
        &mut gauges,
        "freellama_admitted_total",
        "Managed tasks admitted.",
        "admitted",
        true,
    );
    per_backend(
        &mut gauges,
        "freellama_queue_full_rejections_total",
        "Tasks refused because the queue was full.",
        "queue_full_rejections",
        true,
    );
    per_backend(
        &mut gauges,
        "freellama_queue_timeouts_total",
        "Tasks refused after waiting out the queue deadline.",
        "queue_timeouts",
        true,
    );

    let breakers = state.breakers.receipt();
    for (backend, _) in &receipts {
        let open = breakers[*backend]["state"] == "open";
        gauges.push(
            Gauge::new(
                "freellama_circuit_open",
                "1 while the upstream circuit breaker is open.",
                if open { 1.0 } else { 0.0 },
            )
            .label("backend", *backend),
        );
    }
    for (backend, _) in &receipts {
        gauges.push(
            Gauge::new(
                "freellama_adaptive_concurrency_enabled",
                "1 when adaptive concurrency controls this backend.",
                if adaptive_enabled(tunables.adaptive_concurrency, backend) {
                    1.0
                } else {
                    0.0
                },
            )
            .label("backend", *backend),
        );
    }

    let raw = state.raw_admission.receipt();
    for (name, help, field) in [
        (
            "freellama_raw_limit",
            "Raw passthrough concurrency limit.",
            "limit",
        ),
        (
            "freellama_raw_active",
            "Raw passthrough requests streaming now.",
            "active",
        ),
        (
            "freellama_raw_waiting",
            "Raw passthrough requests waiting for a slot.",
            "waiting",
        ),
    ] {
        gauges.push(Gauge::new(name, help, raw[field].as_f64().unwrap_or(0.0)));
    }

    let observation = &snapshot.observation;
    let host = [
        (
            "freellama_host_memory_total_bytes",
            "Physical (or cgroup-limited) memory.",
            observation.total_memory_bytes.map(float),
        ),
        (
            "freellama_host_memory_available_bytes",
            "Memory the OS reports as available.",
            observation.available_memory_bytes.map(float),
        ),
        (
            "freellama_host_memory_effective_available_bytes",
            "Available memory minus FreeLlama reservations.",
            snapshot.effective_available_bytes.map(float),
        ),
        (
            "freellama_host_memory_reserved_bytes",
            "Memory reserved for loads in progress.",
            Some(float(snapshot.reserved_bytes)),
        ),
        (
            "freellama_host_memory_psi_some_avg10",
            "Linux PSI memory some avg10 (percent).",
            observation.memory_psi_some_avg10,
        ),
        (
            "freellama_host_load_average_1m",
            "One-minute load average.",
            observation.load_average_one_minute,
        ),
        (
            "freellama_gpu_memory_total_bytes",
            "Discrete GPU memory (nvidia-smi or amdgpu).",
            observation.gpu_memory_total_bytes.map(float),
        ),
        (
            "freellama_gpu_memory_free_bytes",
            "Free discrete GPU memory.",
            observation.gpu_memory_free_bytes.map(float),
        ),
        (
            "freellama_host_holding",
            "1 while the resource governor holds new loads.",
            Some(if snapshot.holding { 1.0 } else { 0.0 }),
        ),
    ];
    for (name, help, value) in host {
        if let Some(value) = value {
            gauges.push(Gauge::new(name, help, value));
        }
    }

    let loaded = loaded_models(&state).await;
    let model_rows: Vec<&Value> = loaded
        .iter()
        .filter(|row| row.get("name").is_some())
        .collect();
    for (name, help, field) in [
        (
            "freellama_loaded_model_bytes",
            "Memory of a loaded runner as reported by Ollama /api/ps.",
            "size_bytes",
        ),
        (
            "freellama_loaded_model_vram_bytes",
            "Part of a loaded runner held in GPU memory.",
            "size_vram_bytes",
        ),
        (
            "freellama_loaded_model_context_tokens",
            "Context length a loaded runner was started with.",
            "context_length",
        ),
    ] {
        for row in &model_rows {
            gauges.push(
                Gauge::new(name, help, row[field].as_f64().unwrap_or(0.0))
                    .label("backend", row["backend"].as_str().unwrap_or_default())
                    .label("model", row["name"].as_str().unwrap_or_default()),
            );
        }
    }
    gauges.push(Gauge::new(
        "freellama_ollama_num_parallel",
        "OLLAMA_NUM_PARALLEL of the primary Ollama (probed, else Ollama's default).",
        float(state.ollama.num_parallel()),
    ));

    let body = state.telemetry.render_prometheus(&gauges);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
