//! Installed-model catalog: tags cached for 30s, endpoint/digest show metadata for five minutes,
//! fresh residency, and KV-cache estimates. Metadata freshness is independent of LRU access.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::http::StatusCode;
use reqwest::Client;
use serde_json::{Value, json};

use super::config::platform_control_timeout;
use super::discovery::parse_capability;
use super::error::ApiError;
use super::{Backend, CatalogModel, float, ollama_env};
use super::{CatalogCache, PlatformState};
use crate::capability::Capability;

#[path = "catalog_metadata.rs"]
mod metadata;
pub(super) use metadata::MetadataCache;

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
    if let Some(values) = show.pointer("/thinking/values").and_then(Value::as_array) {
        if values.iter().any(|value| {
            value == &Value::Bool(true) || value.as_str().is_some_and(|level| !level.is_empty())
        }) {
            routed.insert(Capability::Thinking);
        } else if values.as_slice() == [Value::Bool(false)] {
            routed.remove(&Capability::Thinking);
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

/// A pinned request needs evidence from its assigned endpoint only. An unrelated failed backend
/// must neither prevent it from running nor add that backend's control timeout to its latency.
pub(crate) async fn discover_for_route(
    state: &PlatformState,
    pinned_model: Option<&str>,
) -> Result<Vec<CatalogModel>, ApiError> {
    let Some(requested) = pinned_model else {
        return discover_models(state).await;
    };
    state.refresh_ollama_settings().await;
    let name = assigned_pin_name(&state.cpu_models, requested);
    // Family-only aliases can span both backends; keep the router's global ambiguity rules.
    if name == requested
        && !requested.contains(':')
        && state.cpu_models.iter().any(|name| {
            name.split_once(':')
                .is_some_and(|(family, _)| family == requested)
        })
    {
        return discover_models(state).await;
    }
    let upstream = if state.cpu_models.contains(&name) {
        state.cpu_upstream.as_deref().unwrap_or(&state.upstream)
    } else {
        &state.upstream
    };
    let pinned = model_from_endpoint(state, upstream, &name, true)
        .await
        .map_err(|error| {
            if error.status().is_client_error() {
                error
            } else {
                endpoint_error(upstream, &error)
            }
        })?;
    match pinned {
        Some(model) => Ok(vec![model]),
        // The endpoint has no such model. Returning an empty list would make the router report
        // "installed: none" on a machine with many models; hand it the real catalog so the
        // refusal names what is installed (or truthfully reports that nothing is).
        None => discover_models(state).await,
    }
}

/// Operator assignments use installed tags. Resolve an omitted `:latest` before selecting the
/// endpoint; a family-only fallback needs a combined installed catalog, not configuration alone.
fn assigned_pin_name(cpu_models: &BTreeSet<String>, requested: &str) -> String {
    if !cpu_models.contains(requested) && !requested.contains(':') {
        let latest = format!("{requested}:latest");
        if cpu_models.contains(&latest) {
            return latest;
        }
    }
    requested.to_owned()
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
    let (models, cacheable) = fetch_catalog(state).await?;
    *state.catalog_cache.write().await = cacheable.then(|| (Instant::now(), models.clone()));
    Ok(models)
}

async fn fetch_catalog(state: &PlatformState) -> Result<(Vec<CatalogModel>, bool), ApiError> {
    let (gpu, cpu) = tokio::join!(fetch_catalog_from(state, &state.upstream), async {
        match &state.cpu_upstream {
            Some(upstream) => Some(fetch_catalog_from(state, upstream).await),
            None => None,
        }
    });
    // A failed confirmation may serve this request once, but must not enter the longer-lived
    // combined snapshot. Failed endpoints themselves contribute no metadata to that snapshot.
    let cacheable = gpu.as_ref().map_or(true, |(_, cacheable)| *cacheable)
        && cpu
            .as_ref()
            .is_none_or(|result| result.as_ref().map_or(true, |(_, cacheable)| *cacheable));
    let mut models = merge_backends(
        gpu.map(|(models, _)| models),
        cpu.map(|result| result.map(|(models, _)| models)),
        &state.cpu_models,
    )?;
    models.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((models, cacheable))
}

/// Failed endpoint observations are unavailable, never an invented empty running list.
fn merge_backends(
    gpu: Result<Vec<CatalogModel>, ApiError>,
    cpu: Option<Result<Vec<CatalogModel>, ApiError>>,
    cpu_models: &BTreeSet<String>,
) -> Result<Vec<CatalogModel>, ApiError> {
    match (gpu, cpu) {
        (Ok(mut gpu), cpu) => {
            gpu.retain(|model| !cpu_models.contains(&model.name));
            match cpu {
                Some(Ok(mut cpu)) => {
                    cpu.retain(|model| cpu_models.contains(&model.name));
                    gpu.extend(cpu);
                }
                Some(Err(error)) => eprintln!("CPU catalog unavailable: {}", error.body.error),
                None => {}
            }
            Ok(gpu)
        }
        (Err(error), Some(Ok(mut cpu))) => {
            eprintln!("primary catalog unavailable: {}", error.body.error);
            cpu.retain(|model| cpu_models.contains(&model.name));
            Ok(cpu)
        }
        (Err(error), _) => Err(error),
    }
}

async fn fetch_catalog_from(
    state: &PlatformState,
    upstream: &str,
) -> Result<(Vec<CatalogModel>, bool), ApiError> {
    let (tags, ps) = tokio::join!(
        get_json(&state.client, upstream, "/api/tags"),
        get_json(&state.client, upstream, "/api/ps")
    );
    let tags = tags?;
    let resident = running_models(ps?)?;
    let entries = tags
        .get("models")
        .and_then(Value::as_array)
        .context("Ollama tags response has no models")
        .map_err(ApiError::upstream)?;
    let entries = entries
        .iter()
        .filter_map(|entry| {
            let name = crate::ollama_http::ps_entry_name(entry).filter(|name| !name.is_empty());
            if name.is_none() {
                eprintln!("skipping malformed Ollama tag: model has no name");
            }
            name.map(|name| (entry, name))
        })
        .collect::<Vec<_>>();
    let shows = show_models(state, upstream, &entries).await;
    let mut models = Vec::with_capacity(entries.len());
    let mut cacheable = true;
    for ((entry, name), show) in entries.into_iter().zip(shows) {
        // An automatic pass may skip a tag without metadata or one replaced mid-pass; the next
        // pass observes its new revision. Pinned requests surface the change as a typed error.
        let show = match show {
            ShowOutcome::Metadata(show) => show,
            ShowOutcome::UnconfirmedMetadata(show) => {
                cacheable = false;
                show
            }
            ShowOutcome::Changed | ShowOutcome::Unavailable => continue,
        };
        models.push(catalog_model(state, entry, name, show.as_ref(), &resident));
    }
    Ok((models, cacheable))
}

/// One catalog entry from Ollama's `tags` entry, its `show` metadata and the running list.
fn catalog_model(
    state: &PlatformState,
    entry: &Value,
    name: &str,
    show: &Value,
    resident: &[Value],
) -> CatalogModel {
    let (capabilities, other_capabilities) = split_capabilities(show);
    let advertised_context = advertised_context_from_show(show);
    let kv_cache_bytes_per_token_f16 = estimate_kv_cache_bytes_per_token_f16(show);
    let modelfile_num_ctx = ollama_env::modelfile_num_ctx(show);
    let architecture = show
        .pointer("/model_info/general.architecture")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let digest = entry
        .get("digest")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let running = matching_resident(resident, name, digest.as_deref());
    let benchmark = state.benchmark.for_model(name, digest.as_deref());
    let policy_rank = state.policies.ranks_for_model(name, digest.as_deref());
    CatalogModel {
        observed_loops: state.model_facts.loops_for(name, digest.as_deref()),
        learned_output_tokens: state.model_facts.output_need(name, digest.as_deref()),
        bytes_per_token_milli: state
            .model_facts
            .bytes_per_token(name, digest.as_deref())
            .map(|(milli, _)| milli),
        bytes_per_token_samples: state
            .model_facts
            .bytes_per_token(name, digest.as_deref())
            .map_or(0, |(_, samples)| samples),
        vision_tokens: super::context::vision_token_bounds(show),
        digest,
        name: name.to_owned(),
        size: entry.get("size").and_then(Value::as_u64).unwrap_or(0),
        capabilities,
        other_capabilities,
        advertised_context,
        kv_cache_bytes_per_token_f16,
        modelfile_num_ctx,
        architecture,
        thinking: show
            .get("thinking")
            .filter(|value| value.is_object())
            .cloned(),
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
        benchmark,
        policy_options: state
            .policies
            .model_options
            .iter()
            .filter_map(|(task, models)| models.get(name).map(|options| (*task, *options)))
            .collect(),
        policy_rank,
    }
}

/// One installed model of the primary backend, for sizing a raw passthrough load: from the shared
/// catalog while it is fresh, else from that backend alone (its tags, running list and one `show`),
/// so a raw request never waits on the CPU backend or on every model's metadata.
pub(crate) async fn primary_model(state: &PlatformState, name: &str) -> Option<CatalogModel> {
    model_from_endpoint(state, &state.upstream, name, false)
        .await
        .ok()
        .flatten()
}

async fn model_from_endpoint(
    state: &PlatformState,
    upstream: &str,
    requested: &str,
    family_fallback: bool,
) -> Result<Option<CatalogModel>, ApiError> {
    if let Some(models) = snapshot_catalog(&state.catalog_cache).await {
        let models: Vec<_> = models
            .into_iter()
            .filter(|model| {
                state.cpu_models.contains(&model.name)
                    == state
                        .cpu_upstream
                        .as_deref()
                        .is_some_and(|cpu| cpu == upstream)
            })
            .collect();
        let names: Vec<_> = models.iter().map(|model| model.name.as_str()).collect();
        if let Some(name) = endpoint_name(&names, requested, family_fallback)?
            && let Some(mut model) = models.iter().find(|model| model.name == name).cloned()
        {
            let resident = running_models(get_json(&state.client, upstream, "/api/ps").await?)?;
            update_residency(state, &mut model, &resident);
            return Ok(Some(model));
        }
    }
    let (tags, ps) = tokio::join!(
        get_json(&state.client, upstream, "/api/tags"),
        get_json(&state.client, upstream, "/api/ps")
    );
    let tags = tags?;
    let resident = running_models(ps?)?;
    let entries = tags["models"]
        .as_array()
        .context("Ollama tags response has no models")
        .map_err(ApiError::upstream)?;
    let names: Vec<_> = entries
        .iter()
        .filter_map(crate::ollama_http::ps_entry_name)
        .collect();
    let Some(name) = endpoint_name(&names, requested, family_fallback)? else {
        return Ok(None);
    };
    let Some(entry) = entries
        .iter()
        .find(|entry| crate::ollama_http::ps_entry_name(entry) == Some(name))
    else {
        return Ok(None);
    };
    match show_models(state, upstream, &[(entry, name)])
        .await
        .pop()
        .unwrap_or(ShowOutcome::Unavailable)
    {
        ShowOutcome::Metadata(show) | ShowOutcome::UnconfirmedMetadata(show) => Ok(Some(catalog_model(
            state,
            entry,
            name,
            show.as_ref(),
            &resident,
        ))),
        ShowOutcome::Changed => Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "installed model {name} changed while its metadata was read; refusing to attach old metadata to a new revision"
            ),
        )
        .with_code("model_identity_changed")),
        ShowOutcome::Unavailable => Err(ApiError::upstream(format!(
            "metadata unavailable for installed model {name}"
        ))),
    }
}

fn endpoint_name<'a>(
    names: &[&'a str],
    requested: &str,
    family_fallback: bool,
) -> Result<Option<&'a str>, ApiError> {
    if family_fallback {
        crate::ollama_http::resolve_listed(names, requested).map_err(|family| {
            ApiError::bad_request(format!(
                "model name is ambiguous: {requested} (installed: {})",
                family.join(", ")
            ))
        })
    } else {
        Ok(crate::ollama_http::resolve_as_ollama(names, requested))
    }
}

/// Text context belongs to the declared architecture, not an auxiliary vision encoder.
/// Metadata without an architecture is usable only when there is one unambiguous context field.
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

/// Each catalog endpoint fill spawns at most four workers; the shared metadata cache also limits
/// active `/show` requests to four across endpoints and independent pinned discovery calls.
const SHOW_CONCURRENCY: usize = 4;
const MAX_SHOW_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct ShowLookup {
    show: Arc<Value>,
    fetched: bool,
}

/// What one tag's `/api/show` pass established about the revision `/api/tags` listed.
enum ShowOutcome {
    /// Metadata for the listed revision, or a tag that did not advertise an artifact digest.
    Metadata(Arc<Value>),
    /// The listed digest could not be re-checked. Serve once, never cache in either layer.
    UnconfirmedMetadata(Arc<Value>),
    /// The tag now names a different revision (or vanished): the fetched metadata describes
    /// neither the listed digest nor a known one, so it is discarded.
    Changed,
    /// No usable metadata (request failed, malformed or oversized body).
    Unavailable,
}

async fn show_models(
    state: &PlatformState,
    upstream: &str,
    entries: &[(&Value, &str)],
) -> Vec<ShowOutcome> {
    let mut shows = vec![None; entries.len()];
    for (chunk_index, chunk) in entries.chunks(SHOW_CONCURRENCY).enumerate() {
        let mut lookups = tokio::task::JoinSet::new();
        for (offset, (entry, name)) in chunk.iter().enumerate() {
            let (state, upstream, name) = (state.clone(), upstream.to_owned(), (*name).to_owned());
            let digest = entry
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned);
            lookups.spawn(async move {
                (
                    offset,
                    show_model(&state, &upstream, &name, digest.as_deref()).await,
                )
            });
        }
        while let Some(joined) = lookups.join_next().await {
            match joined {
                Ok((offset, show)) => shows[chunk_index * SHOW_CONCURRENCY + offset] = show,
                Err(error) => eprintln!("skipping model metadata lookup: {error}"),
            }
        }
    }
    confirm_show_identities(state, upstream, entries, shows).await
}

/// `/show` is addressed by mutable tag, not by digest. Confirm identities once after a miss batch
/// before caching metadata under the earlier `/tags` digest. Otherwise a concurrent tag update
/// could attribute a new revision's capabilities to old weights. Cache hits need no extra trip.
/// If the confirmation read fails, the fetched metadata serves this request but is not cached; a
/// changed revision is reported as [`ShowOutcome::Changed`], never paired with the old show.
async fn confirm_show_identities(
    state: &PlatformState,
    upstream: &str,
    entries: &[(&Value, &str)],
    shows: Vec<Option<ShowLookup>>,
) -> Vec<ShowOutcome> {
    let listed_digest = |entry: &Value| {
        entry
            .get("digest")
            .and_then(Value::as_str)
            .filter(|digest| !digest.trim().is_empty())
            .map(str::to_owned)
    };
    let needs_confirmation = entries.iter().zip(&shows).any(|((entry, _), lookup)| {
        lookup.as_ref().is_some_and(|lookup| lookup.fetched) && listed_digest(entry).is_some()
    });
    let current = if needs_confirmation {
        get_json(&state.client, upstream, "/api/tags").await.ok()
    } else {
        None
    };
    let current = current
        .as_ref()
        .and_then(|tags| tags.get("models"))
        .and_then(Value::as_array);
    entries
        .iter()
        .zip(shows)
        .map(|((entry, name), lookup)| {
            let Some(found) = lookup else {
                return ShowOutcome::Unavailable;
            };
            let Some(digest) = listed_digest(entry).filter(|_| found.fetched) else {
                return ShowOutcome::Metadata(found.show);
            };
            let Some(models) = current else {
                eprintln!("model {name}: revision not confirmed after /api/show; not caching");
                return ShowOutcome::UnconfirmedMetadata(found.show);
            };
            let same_revision = models
                .iter()
                .find(|model| crate::ollama_http::ps_entry_name(model) == Some(*name))
                .and_then(|model| model.get("digest"))
                .and_then(Value::as_str)
                == Some(digest.as_str());
            if same_revision {
                state
                    .catalog_metadata
                    .insert(upstream, Some(&digest), Arc::clone(&found.show));
                ShowOutcome::Metadata(found.show)
            } else {
                eprintln!("model {name}: revision changed after /api/show");
                ShowOutcome::Changed
            }
        })
        .collect()
}

/// `None` means skip this tag — a single corrupt `/api/show` must not 502 the whole catalog.
async fn show_model(
    state: &PlatformState,
    upstream: &str,
    name: &str,
    digest: Option<&str>,
) -> Option<ShowLookup> {
    if let Some(show) = state.catalog_metadata.get(upstream, digest) {
        return Some(ShowLookup {
            show,
            fetched: false,
        });
    }
    let _fetch = state.catalog_metadata.fetch_slot().await?;
    if let Some(show) = state.catalog_metadata.get(upstream, digest) {
        return Some(ShowLookup {
            show,
            fetched: false,
        });
    }
    let fetched = async {
        let response = state
            .client
            .post(crate::ollama_http::url(upstream, "/api/show"))
            .timeout(platform_control_timeout())
            .json(&json!({"model": name, "verbose": false}))
            .send()
            .await?
            .error_for_status()?;
        let value = bounded_show_json(response).await?;
        metadata::compact_show(value).context("malformed /api/show metadata")
    }
    .await;
    match fetched {
        Ok(show) => Some(ShowLookup {
            show: Arc::new(show),
            fetched: true,
        }),
        Err(error) => {
            eprintln!("skipping model {name}: {error:#}");
            None
        }
    }
}

async fn bounded_show_json(mut response: reqwest::Response) -> Result<Value> {
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_SHOW_BODY_BYTES as u64),
        "/api/show body exceeds metadata limit"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= MAX_SHOW_BODY_BYTES.saturating_sub(body.len()),
            "/api/show body exceeds metadata limit"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("invalid /api/show JSON")
}

async fn refresh_residency(
    state: &PlatformState,
    models: &mut Vec<CatalogModel>,
) -> Result<(), ApiError> {
    let (gpu, cpu) = backend_residency(state).await;
    let cpu = cpu.map(|result| result.and_then(running_models));
    let gpu = match gpu.and_then(running_models) {
        Err(error) if !matches!(cpu, Some(Ok(_))) => return Err(error),
        other => other,
    };
    // Drop only the failed endpoint's candidates. A cached positive residency observation must
    // not survive a failed live observation, and an unknown observation is not `resident=false`.
    models.retain_mut(|model| {
        let running = if state.cpu_models.contains(&model.name) {
            cpu.as_ref().and_then(|result| result.as_ref().ok())
        } else {
            gpu.as_ref().ok()
        };
        let Some(running) = running else { return false };
        update_residency(state, model, running);
        true
    });
    Ok(())
}

fn running_models(ps: Value) -> Result<Vec<Value>, ApiError> {
    let models = match ps {
        Value::Object(mut object) => object.remove("models"),
        _ => None,
    };
    let Some(Value::Array(models)) = models else {
        return Err(ApiError::upstream(
            "Ollama /api/ps response has no models array; residency is unknown",
        ));
    };
    if models
        .iter()
        .any(|entry| crate::ollama_http::ps_entry_name(entry).is_none_or(str::is_empty))
    {
        return Err(ApiError::upstream(
            "Ollama /api/ps contains an unnamed model; residency is unknown",
        ));
    }
    Ok(models)
}

fn matching_resident<'a>(
    resident: &'a [Value],
    name: &str,
    digest: Option<&str>,
) -> Option<&'a Value> {
    resident.iter().find(|running| {
        crate::ollama_http::ps_entry_name(running) == Some(name)
            && match (digest, running.get("digest").and_then(Value::as_str)) {
                (Some(expected), Some(actual)) => expected == actual,
                _ => true,
            }
    })
}

fn update_residency(state: &PlatformState, model: &mut CatalogModel, running: &[Value]) {
    let resident = matching_resident(running, &model.name, model.digest.as_deref());
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
    // Live like residency: static metadata must not hide observations learned since its fill.
    model.observed_loops = state
        .model_facts
        .loops_for(&model.name, model.digest.as_deref());
    model.learned_output_tokens = state
        .model_facts
        .output_need(&model.name, model.digest.as_deref());
    let ratio = state
        .model_facts
        .bytes_per_token(&model.name, model.digest.as_deref());
    model.bytes_per_token_milli = ratio.map(|(milli, _)| milli);
    model.bytes_per_token_samples = ratio.map_or(0, |(_, samples)| samples);
}

fn endpoint_error(upstream: &str, error: &ApiError) -> ApiError {
    ApiError::upstream(format!(
        "model catalog unavailable at {upstream}: {}",
        error.body.error
    ))
    .with_code("catalog_endpoint_unavailable")
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
#[path = "catalog_tests.rs"]
mod cache_tests;

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
            thinking: None,
            resident: false,
            resident_size: None,
            resident_vram: None,
            resident_context: None,
            benchmark: BTreeMap::new(),
            observed_loops: BTreeMap::new(),
            learned_output_tokens: None,
            bytes_per_token_milli: None,
            bytes_per_token_samples: 0,
            vision_tokens: None,
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
