//! Installed-model catalog: Ollama tags, show metadata, residency and KV-cache estimates.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Value, json};

use super::config::platform_control_timeout;
use super::discovery::parse_capability;
use super::error::ApiError;
use super::{Backend, CatalogModel, float, ollama_env};
use super::{CatalogCache, PlatformState};
use crate::capability::Capability;

/// Ollama's `capabilities`: those routing uses, and the rest verbatim so none is silently dropped.
fn split_capabilities(show: &Value) -> (BTreeSet<Capability>, Vec<String>) {
    let reported = show
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let mut routed = BTreeSet::new();
    let mut other = Vec::new();
    for value in reported {
        match parse_capability(value) {
            Some(capability) => {
                routed.insert(capability);
            }
            None => other.push(value.to_owned()),
        }
    }
    (routed, other)
}

pub(crate) async fn discover_models(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    state.refresh_ollama_settings().await;
    if let Some(mut models) = snapshot_catalog(&state.catalog_cache).await {
        refresh_residency(state, &mut models).await?;
        return Ok(models);
    }
    fill_catalog(state).await
}

async fn snapshot_catalog(cache: &CatalogCache) -> Option<Vec<CatalogModel>> {
    cache
        .read()
        .await
        .as_ref()
        .filter(|(saved, _)| saved.elapsed() < Duration::from_secs(30))
        .map(|(_, models)| models.clone())
}

/// Singleflight fill: concurrent cache misses share one tags+ps+per-model show pass.
async fn fill_catalog(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    let fill = state.catalog_refresh.lock().await;
    if let Some(mut models) = snapshot_catalog(&state.catalog_cache).await {
        drop(fill);
        refresh_residency(state, &mut models).await?;
        return Ok(models);
    }
    let models = fetch_catalog(state).await?;
    *state.catalog_cache.write().await = Some((Instant::now(), models.clone()));
    Ok(models)
}

async fn fetch_catalog(state: &PlatformState) -> Result<Vec<CatalogModel>, ApiError> {
    let (gpu, cpu) = tokio::join!(fetch_catalog_from(state, &state.upstream), async {
        match &state.cpu_upstream {
            Some(upstream) => Some(fetch_catalog_from(state, upstream).await),
            None => None,
        }
    });
    let mut models = gpu?;
    models.retain(|model| !state.cpu_models.contains(&model.name));
    if let Some(cpu) = cpu {
        let mut cpu_models = cpu?;
        cpu_models.retain(|model| state.cpu_models.contains(&model.name));
        models.extend(cpu_models);
    }
    models.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(models)
}

async fn fetch_catalog_from(
    state: &PlatformState,
    upstream: &str,
) -> Result<Vec<CatalogModel>, ApiError> {
    let (tags, ps) = tokio::join!(
        get_json(&state.client, upstream, "/api/tags"),
        get_json(&state.client, upstream, "/api/ps")
    );
    let tags = tags?;
    let ps = ps?;
    let resident = ps
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries = tags
        .get("models")
        .and_then(Value::as_array)
        .context("Ollama tags response has no models")
        .map_err(ApiError::upstream)?;
    let names = entries
        .iter()
        .map(|entry| {
            crate::ollama_http::ps_entry_name(entry)
                .context("Ollama model has no name")
                .map_err(ApiError::upstream)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut shows = show_models(state, upstream, &names).await?;
    let mut models = Vec::with_capacity(entries.len());
    for (index, (entry, name)) in entries.iter().zip(names).enumerate() {
        let Some(show) = shows[index].take() else {
            continue;
        };
        let (capabilities, other_capabilities) = split_capabilities(&show);
        let advertised_context = advertised_context_from_show(&show);
        let kv_cache_bytes_per_token_f16 = estimate_kv_cache_bytes_per_token_f16(&show);
        let modelfile_num_ctx = ollama_env::modelfile_num_ctx(&show);
        let architecture = show
            .pointer("/model_info/general.architecture")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let running = resident
            .iter()
            .find(|running| crate::ollama_http::ps_entry_name(running) == Some(name));
        models.push(CatalogModel {
            digest: entry
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: name.to_owned(),
            size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
            capabilities,
            other_capabilities,
            advertised_context,
            kv_cache_bytes_per_token_f16,
            modelfile_num_ctx,
            architecture,
            resident: running.is_some(),
            resident_size: running
                .and_then(|value| value.get("size"))
                .and_then(Value::as_u64),
            resident_vram: running
                .and_then(|value| value.get("size_vram"))
                .and_then(Value::as_u64),
            resident_context: running
                .and_then(|value| value.get("context_length"))
                .and_then(Value::as_u64),
            benchmark: state.benchmark.get(name).cloned().unwrap_or_default(),
            observed_loops: state.loop_evidence.for_model(name),
            policy_options: state
                .policies
                .model_options
                .iter()
                .filter_map(|(task, models)| models.get(name).map(|options| (*task, *options)))
                .collect(),
            policy_rank: state
                .policies
                .qualified
                .iter()
                .filter_map(|(task, candidates)| {
                    candidates
                        .iter()
                        .position(|candidate| candidate == name)
                        .map(|rank| (*task, rank))
                })
                .collect(),
        });
    }
    Ok(models)
}

/// Text context belongs to the declared architecture, not an auxiliary vision encoder. Legacy
/// metadata without an architecture is usable only when there is one unambiguous context field.
fn advertised_context_from_show(show: &Value) -> Option<u64> {
    let info = show.get("model_info")?.as_object()?;
    let value = if let Some(architecture) = info.get("general.architecture") {
        info.get(&format!("{}.context_length", architecture.as_str()?))?
    } else {
        let mut values = info
            .iter()
            .filter(|(key, _)| key.ends_with(".context_length"));
        let (_, value) = values.next()?;
        if values.next().is_some() {
            return None;
        }
        value
    };
    value.as_u64().filter(|context| *context > 0)
}

/// Estimate unpadded F16 K+V bytes per token for a uniform full-attention layout, read from the
/// model's own metadata whatever its architecture. This is not live allocation or a lower bound
/// across cache precisions. Ollama additionally handles padding, parallel slots, recurrent state,
/// sliding windows and architecture-specific caches: metadata that declares any of those (or
/// per-layer head counts, which are arrays) returns `None` rather than a dense formula that
/// would be wrong for it.
fn estimate_kv_cache_bytes_per_token_f16(show: &Value) -> Option<u64> {
    let info = show.get("model_info")?.as_object()?;
    let architecture = info.get("general.architecture")?.as_str()?;
    let prefix = format!("{architecture}.");
    if info
        .keys()
        .filter_map(|key| key.strip_prefix(&prefix))
        .any(|key| {
            key.starts_with("ssm.")
                || key.starts_with("wkv")
                || key.contains("recurrent")
                || key.contains("sliding_window")
                || key.contains("shared_kv")
                || key.contains("cross_attention")
        })
    {
        return None;
    }
    // Scope every field to the declared text architecture; multimodal metadata can also contain
    // clip.block_count or other matching suffixes which describe a different network.
    let field = |name: &str| info.get(&format!("{prefix}{name}"));
    let positive = |value: &Value| value.as_u64().filter(|value| *value > 0);
    let blocks = positive(field("block_count")?)?;
    let heads = positive(field("attention.head_count")?)?;
    let kv_heads = positive(field("attention.head_count_kv")?)?;
    let dimension = |name: &str| {
        if let Some(value) = field(name) {
            positive(value)
        } else {
            let embedding = positive(field("embedding_length")?)?;
            (embedding % heads == 0).then_some(embedding / heads)
        }
    };
    let key = dimension("attention.key_length")?;
    let value = dimension("attention.value_length")?;
    blocks
        .checked_mul(kv_heads)?
        .checked_mul(key.checked_add(value)?)?
        .checked_mul(2)
}

/// Concurrent `/api/show` lookups for a whole catalog, so a cold catalog does not cost one serial
/// round trip per installed model; a small bound keeps a large library from flooding Ollama. Results keep the order of `names`.
const SHOW_CONCURRENCY: usize = 4;

async fn show_models(
    state: &PlatformState,
    upstream: &str,
    names: &[&str],
) -> Result<Vec<Option<Value>>, ApiError> {
    let permits = Arc::new(tokio::sync::Semaphore::new(SHOW_CONCURRENCY));
    let mut lookups = tokio::task::JoinSet::new();
    for (index, name) in names.iter().enumerate() {
        let (state, upstream, name) = (state.clone(), upstream.to_owned(), (*name).to_owned());
        let permits = Arc::clone(&permits);
        lookups.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (index, show_model(&state, &upstream, &name).await)
        });
    }
    let mut shows = vec![None; names.len()];
    while let Some(joined) = lookups.join_next().await {
        let (index, show) = joined
            .context("model metadata lookup panicked")
            .map_err(ApiError::upstream)?;
        shows[index] = show?;
    }
    Ok(shows)
}

/// `None` means skip this tag — a single corrupt `/api/show` must not 502 the whole catalog.
async fn show_model(
    state: &PlatformState,
    upstream: &str,
    name: &str,
) -> Result<Option<Value>, ApiError> {
    match state
        .client
        .post(crate::ollama_http::url(upstream, "/api/show"))
        .timeout(platform_control_timeout())
        .json(&json!({"model": name}))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => Ok(Some(
            response.json::<Value>().await.map_err(ApiError::upstream)?,
        )),
        Ok(response) => {
            eprintln!(
                "skipping model {name}: /api/show returned {}",
                response.status()
            );
            Ok(None)
        }
        Err(error) => {
            eprintln!("skipping model {name}: {error:#}");
            Ok(None)
        }
    }
}

async fn refresh_residency(
    state: &PlatformState,
    models: &mut [CatalogModel],
) -> Result<(), ApiError> {
    let (gpu, cpu) = backend_residency(state).await;
    let gpu = gpu?;
    let gpu_running = gpu
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cpu_running = if let Some(cpu) = cpu {
        cpu?.get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    for model in models {
        let running = if state.cpu_models.contains(&model.name) {
            &cpu_running
        } else {
            &gpu_running
        };
        let resident = running
            .iter()
            .find(|entry| crate::ollama_http::ps_entry_name(entry) == Some(model.name.as_str()));
        model.resident = resident.is_some();
        model.resident_size = resident
            .and_then(|value| value.get("size"))
            .and_then(Value::as_u64);
        model.resident_vram = resident
            .and_then(|value| value.get("size_vram"))
            .and_then(Value::as_u64);
        model.resident_context = resident
            .and_then(|value| value.get("context_length"))
            .and_then(Value::as_u64);
        // Live like residency: cached static metadata must not hide a loop seen since.
        model.observed_loops = state.loop_evidence.for_model(&model.name);
    }
    Ok(())
}

/// Independent runner observations share a deadline window rather than adding backend delays.
pub(crate) async fn backend_residency(
    state: &PlatformState,
) -> (Result<Value, ApiError>, Option<Result<Value, ApiError>>) {
    tokio::join!(get_json(&state.client, &state.upstream, "/api/ps"), async {
        match state.cpu_upstream.as_deref() {
            Some(upstream) => Some(get_json(&state.client, upstream, "/api/ps").await),
            None => None,
        }
    })
}

pub(crate) async fn get_json(
    client: &Client,
    upstream: &str,
    path: &str,
) -> Result<Value, ApiError> {
    client
        .get(crate::ollama_http::url(upstream, path))
        .timeout(platform_control_timeout())
        .send()
        .await
        .map_err(ApiError::upstream)?
        .error_for_status()
        .map_err(ApiError::upstream)?
        .json()
        .await
        .map_err(ApiError::upstream)
}

/// The GPU/CPU split display for one resident model, as the `resident_view` inventory field.
/// Ollama's `/api/ps` exposes only the raw `size`/`size_vram` bytes; the `ollama ps` CLI derives
/// the split, the API does not. `None` for a non-resident model or one without numeric resident
/// bytes and a nonzero size; such entries carry no derived view.
pub(super) fn resident_view(
    placement: Backend,
    resident: bool,
    size: Option<u64>,
    vram: Option<u64>,
    observation: &Value,
) -> Option<Value> {
    let (size, vram) = (size?, vram?);
    if !resident || size == 0 {
        return None;
    }
    let gpu_percent = rounded_percent(vram, size);
    let processor = match observation.get("processor").and_then(Value::as_str) {
        Some(observed @ ("cpu" | "gpu" | "mixed")) => observed,
        _ if gpu_percent >= 100 => "gpu",
        _ if gpu_percent == 0 => "cpu",
        _ => "mixed",
    };
    let processor_label = match processor {
        "gpu" => "100% GPU".to_owned(),
        "cpu" => "100% CPU".to_owned(),
        _ => format!(
            "{gpu_percent}% GPU / {}% CPU",
            100_u64.saturating_sub(gpu_percent)
        ),
    };
    let mut view = json!({
        "gpu_percent": gpu_percent,
        "processor": processor,
        "processor_label": processor_label,
        "assigned": placement == Backend::Cpu,
    });
    if observation.get("status").and_then(Value::as_str) == Some("mismatch") {
        view["warning"] = json!(format!(
            "Configured {placement} backend disagrees with Ollama /api/ps: {processor} was physically observed. This sample is excluded from adaptive routing feedback."
        ));
    } else if processor == "mixed" {
        view["warning"] = json!(
            "Partially offloaded to CPU — expect a large slowdown. Free VRAM (`ollama_manage` action \"stop\") or lower the context length."
        );
    }
    Some(view)
}

/// `round(vram / size * 100)` with the same floating-point result as the `JavaScript`
/// `Math.round` it replaces: the exact integer rounding can differ by one where the float ratio
/// lands just below a `.5` boundary, so pick the neighbour that equals the float result.
fn rounded_percent(vram: u64, size: u64) -> u64 {
    let target = (float(vram) / float(size) * 100.0).round();
    let exact = (u128::from(vram) * 200 + u128::from(size)) / (u128::from(size) * 2);
    let exact = u64::try_from(exact).unwrap_or(u64::MAX);
    [exact, exact.saturating_sub(1), exact.saturating_add(1)]
        .into_iter()
        .find(|candidate| float(*candidate).total_cmp(&target).is_eq())
        .unwrap_or(exact)
}

#[cfg(test)]
mod resident_view_tests {
    use super::super::physical_placement_observation as observation_for;
    use super::*;

    fn view(placement: Backend, size: Option<u64>, vram: Option<u64>) -> Option<Value> {
        resident_view(
            placement,
            true,
            size,
            vram,
            &observation_for(placement, size, vram),
        )
    }

    #[test]
    fn full_gpu_full_cpu_and_mixed_splits() {
        assert_eq!(
            view(Backend::Gpu, Some(1000), Some(1000)).unwrap(),
            json!({"gpu_percent": 100, "processor": "gpu", "processor_label": "100% GPU", "assigned": false})
        );
        assert_eq!(
            view(Backend::Cpu, Some(1000), Some(0)).unwrap(),
            json!({"gpu_percent": 0, "processor": "cpu", "processor_label": "100% CPU", "assigned": true})
        );
        let mixed = resident_view(
            Backend::Gpu,
            true,
            Some(2000),
            Some(500),
            &json!({"processor": "mixed", "status": "verified"}),
        )
        .unwrap();
        assert_eq!(
            mixed,
            json!({
                "gpu_percent": 25,
                "processor": "mixed",
                "processor_label": "25% GPU / 75% CPU",
                "assigned": false,
                "warning": "Partially offloaded to CPU — expect a large slowdown. Free VRAM (`ollama_manage` action \"stop\") or lower the context length.",
            })
        );
    }

    #[test]
    fn observed_processor_overrides_the_byte_split() {
        let observed_gpu = resident_view(
            Backend::Gpu,
            true,
            Some(2000),
            Some(500),
            &json!({"processor": "gpu", "status": "verified"}),
        )
        .unwrap();
        assert_eq!(observed_gpu["gpu_percent"], 25);
        assert_eq!(observed_gpu["processor"], "gpu");
        assert_eq!(observed_gpu["processor_label"], "100% GPU");
        assert!(observed_gpu.get("warning").is_none());
        // An unclassified observation falls back to the byte split.
        let fallback = |vram| {
            resident_view(
                Backend::Gpu,
                true,
                Some(1000),
                Some(vram),
                &json!({"processor": "unknown"}),
            )
            .unwrap()["processor"]
                .clone()
        };
        assert_eq!(fallback(1000), "gpu");
        assert_eq!(fallback(0), "cpu");
        assert_eq!(fallback(400), "mixed");
    }

    #[test]
    fn mismatch_warns_about_the_configured_backend() {
        let mismatch = view(Backend::Cpu, Some(1000), Some(1000)).unwrap();
        assert_eq!(mismatch["assigned"], true);
        assert_eq!(mismatch["processor_label"], "100% GPU");
        assert_eq!(
            mismatch["warning"],
            "Configured cpu backend disagrees with Ollama /api/ps: gpu was physically observed. This sample is excluded from adaptive routing feedback."
        );
        let mixed_mismatch = view(Backend::Gpu, Some(2000), Some(500)).unwrap();
        assert_eq!(
            mixed_mismatch["warning"],
            "Configured gpu backend disagrees with Ollama /api/ps: mixed was physically observed. This sample is excluded from adaptive routing feedback."
        );
    }

    #[test]
    fn zero_missing_or_non_resident_sizes_pass_through() {
        assert!(view(Backend::Gpu, Some(0), Some(0)).is_none());
        assert!(view(Backend::Gpu, None, Some(500)).is_none());
        assert!(view(Backend::Gpu, Some(1000), None).is_none());
        assert!(resident_view(Backend::Gpu, false, Some(1000), Some(1000), &json!({})).is_none());
    }

    #[test]
    fn percent_matches_javascript_float_rounding() {
        // 29/200*100 is 14.499999999999998 in IEEE doubles, so Math.round gives 14, not 15.
        assert_eq!(rounded_percent(29, 200), 14);
        assert_eq!(rounded_percent(1, 8), 13);
        assert_eq!(rounded_percent(500, 2000), 25);
        assert_eq!(rounded_percent(1000, 1000), 100);
        assert_eq!(rounded_percent(1, 1000), 0);
    }
}

#[cfg(test)]
mod kv_estimate_tests {
    use super::super::{PlatformConfig, build, memory_kv_preflight_with_memory};
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[tokio::test]
    async fn process_settings_refresh_after_cache_expiry_and_preserve_fresh_cache() {
        let mut config =
            PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11439", None, None);
        config.cpu_upstream = Some("http://127.0.0.1:11438".into());
        config.cpu_models.insert("cpu-helper:latest".into());
        let (_, state) = build(&config).unwrap();
        let old = ollama_env::resolve(
            Some(&BTreeMap::from([(
                "OLLAMA_NUM_PARALLEL".into(),
                "99".into(),
            )])),
            |_| None,
            |_| None,
            "test".into(),
        );
        *state.ollama.write().await = old.clone();
        *state.cpu_ollama.as_ref().unwrap().write().await = old;
        state.refresh_ollama_settings().await;
        assert_eq!(
            state.ollama.read().await.num_parallel(),
            99,
            "fresh observations must be reused"
        );
        *state.ollama_refresh.lock().await =
            Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
        state.refresh_ollama_settings().await;
        assert_ne!(state.ollama.read().await.num_parallel(), 99);
        assert_ne!(
            state
                .cpu_ollama
                .as_ref()
                .unwrap()
                .read()
                .await
                .num_parallel(),
            99
        );
        assert!(state.ollama_refresh.lock().await.elapsed() < Duration::from_secs(5));
    }

    fn llama_shape() -> Value {
        json!({"model_info": {
            "general.architecture": "llama",
            "llama.block_count": 16,
            "llama.attention.head_count": 16,
            "llama.attention.head_count_kv": 8,
            "llama.embedding_length": 1536,
        }})
    }

    #[test]
    fn kv_estimate_respects_explicit_key_and_value_dimensions() {
        let mut show = llama_shape();
        show["model_info"]["llama.attention.key_length"] = json!(128);
        show["model_info"]["llama.attention.value_length"] = json!(64);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]["llama.attention.value_length"] = json!(128);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
        // Explicit dimensions do not require a divisible embedding width.
        show["model_info"]["llama.embedding_length"] = json!(1537);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(65_536));
    }

    #[test]
    fn kv_estimate_never_borrows_another_architectures_fields() {
        let mut show = llama_shape();
        show["model_info"]["clip.block_count"] = json!(999);
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), Some(49_152));
        show["model_info"]
            .as_object_mut()
            .unwrap()
            .remove("llama.block_count");
        assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None);
    }

    #[test]
    fn kv_estimate_declares_nonuniform_or_invalid_shapes_unknown() {
        for (key, value) in [
            ("llama.attention.sliding_window", json!(512)),
            ("llama.attention.shared_kv_layers", json!(18)),
            ("llama.ssm.state_size", json!(16)),
            ("llama.attention.head_count_kv", json!([8, 0])),
            ("llama.attention.key_length", json!("128")),
            ("llama.attention.key_length", json!(0)),
            ("llama.block_count", json!(0)),
            ("llama.block_count", json!(u64::MAX)),
            ("general.architecture", json!("new_unknown_family")),
        ] {
            let mut show = llama_shape();
            show["model_info"][key] = value;
            assert_eq!(estimate_kv_cache_bytes_per_token_f16(&show), None, "{key}");
        }
    }

    #[test]
    fn kv_preflight_does_not_call_f16_a_known_runtime_floor() {
        let model = CatalogModel {
            digest: None,
            name: "test".into(),
            size: 600,
            capabilities: BTreeSet::new(),
            other_capabilities: Vec::new(),
            advertised_context: None,
            kv_cache_bytes_per_token_f16: Some(10),
            modelfile_num_ctx: None,
            architecture: None,
            resident: false,
            resident_size: None,
            resident_vram: None,
            resident_context: None,
            benchmark: BTreeMap::new(),
            observed_loops: BTreeMap::new(),
            policy_rank: BTreeMap::new(),
            policy_options: BTreeMap::new(),
        };
        let report = memory_kv_preflight_with_memory(&model, Some(30), true, Some(1000));
        assert_eq!(report["refuses"], false);
        assert_eq!(report["status"], "f16_estimate_exceeds_host_budget");
        assert_eq!(report["model_plus_kv_bytes_f16_estimate"], 900);
        assert!(report.get("known_runtime_floor_bytes").is_none());
        let mut large = model.clone();
        large.size = 900;
        large.kv_cache_bytes_per_token_f16 = None;
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), true, Some(1000))["refuses"],
            true
        );
        assert_eq!(
            memory_kv_preflight_with_memory(&large, Some(30), false, Some(1000))["refuses"],
            false
        );
    }
}
