//! Catalog cache/failure contracts. Mock HTTP only; no installed model operations.

use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Engine {
    tags: Arc<Mutex<Value>>,
    tags_status: Arc<Mutex<StatusCode>>,
    ps: Arc<Mutex<(StatusCode, Value)>>,
    shows: Arc<Mutex<BTreeMap<String, Value>>>,
    show_calls: Arc<AtomicUsize>,
    ps_calls: Arc<AtomicUsize>,
}

struct Mock {
    url: String,
    engine: Engine,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn mock(name: &str, digest: Option<&str>) -> Mock {
    let mut entry = json!({"name":name,"size":1024});
    if let Some(digest) = digest {
        entry["digest"] = json!(digest);
    }
    let engine = Engine {
        tags: Arc::new(Mutex::new(json!({"models":[entry]}))),
        tags_status: Arc::new(Mutex::new(StatusCode::OK)),
        ps: Arc::new(Mutex::new((StatusCode::OK, json!({"models":[]})))),
        shows: Arc::new(Mutex::new(BTreeMap::from([(
            name.into(),
            json!({
                "capabilities":["completion"],
                "model_info":{"general.architecture":"llama","llama.context_length":4096}
            }),
        )]))),
        show_calls: Arc::new(AtomicUsize::new(0)),
        ps_calls: Arc::new(AtomicUsize::new(0)),
    };
    let router = Router::new()
        .route(
            "/api/tags",
            get(|State(engine): State<Engine>| async move {
                let status = *engine.tags_status.lock().await;
                (status, Json(engine.tags.lock().await.clone()))
            }),
        )
        .route(
            "/api/ps",
            get(|State(engine): State<Engine>| async move {
                engine.ps_calls.fetch_add(1, Ordering::SeqCst);
                let (status, body) = engine.ps.lock().await.clone();
                (status, Json(body))
            }),
        )
        .route(
            "/api/show",
            post(
                |State(engine): State<Engine>, Json(input): Json<Value>| async move {
                    let previous_calls = engine.show_calls.fetch_add(1, Ordering::SeqCst);
                    if matches!(input["model"].as_str(), Some("outage" | "outage-changes"))
                        && previous_calls == 0
                    {
                        *engine.tags_status.lock().await = StatusCode::SERVICE_UNAVAILABLE;
                        if input["model"] == "outage-changes" {
                            engine.tags.lock().await["models"][0]["digest"] = json!("revision-b");
                        }
                    }
                    if input["model"] == "changes" {
                        engine.tags.lock().await["models"][0]["digest"] = json!("revision-b");
                    }
                    if input["model"] == "invalid-json" {
                        return "{broken".into_response();
                    }
                    engine
                        .shows
                        .lock()
                        .await
                        .get(input["model"].as_str().unwrap_or(""))
                        .cloned()
                        .map_or_else(
                            || StatusCode::NOT_FOUND.into_response(),
                            |value| Json(value).into_response(),
                        )
                },
            ),
        )
        .with_state(engine.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Mock {
        url,
        engine,
        server,
    }
}

fn state(gpu: &Mock, cpu: Option<&Mock>) -> PlatformState {
    let mut config = super::super::PlatformConfig::new("127.0.0.1:0", gpu.url.clone(), None, None);
    if let Some(cpu) = cpu {
        config.cpu_upstream = Some(cpu.url.clone());
        config.cpu_models.insert("cpu".into());
    }
    super::super::build(&config).unwrap().1
}

async fn expire_tags(state: &PlatformState) {
    if let Some((saved, _)) = state.catalog_cache.write().await.as_mut() {
        *saved = Instant::now().checked_sub(Duration::from_secs(31)).unwrap();
    }
}

#[tokio::test]
async fn unchanged_digest_reuses_show_after_tags_expire_but_ps_is_fresh() {
    let gpu = mock("gpu", Some("revision-a")).await;
    let state = state(&gpu, None);
    assert!(!discover_models(&state).await.unwrap()[0].resident);
    gpu.engine.ps.lock().await.1 = json!({"models":[{"name":"gpu","digest":"revision-a","size":2048,"size_vram":1024,"context_length":4096}]});
    expire_tags(&state).await;
    let models = discover_models(&state).await.unwrap();
    assert!(models[0].resident);
    assert_eq!(models[0].resident_size, Some(2048));
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gpu.engine.ps_calls.load(Ordering::SeqCst), 2);
    gpu.engine.ps.lock().await.1 = json!({"models":[]});
    assert!(!discover_models(&state).await.unwrap()[0].resident);
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn digest_change_fetches_new_metadata_and_missing_digest_never_reuses_it() {
    let gpu = mock("gpu", Some("revision-a")).await;
    let state = state(&gpu, None);
    discover_models(&state).await.unwrap();
    gpu.engine.tags.lock().await["models"][0]["digest"] = json!("revision-b");
    gpu.engine.shows.lock().await.get_mut("gpu").unwrap()["capabilities"] = json!(["embedding"]);
    expire_tags(&state).await;
    let models = discover_models(&state).await.unwrap();
    assert_eq!(models[0].digest.as_deref(), Some("revision-b"));
    assert!(models[0].capabilities.contains(&Capability::Embedding));
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
    gpu.engine.tags.lock().await["models"][0]
        .as_object_mut()
        .unwrap()
        .remove("digest");
    for _ in 0..2 {
        expire_tags(&state).await;
        discover_models(&state).await.unwrap();
    }
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn identical_digest_on_distinct_endpoints_does_not_share_metadata() {
    let gpu = mock("gpu", Some("same-digest")).await;
    let cpu = mock("cpu", Some("same-digest")).await;
    cpu.engine.shows.lock().await.get_mut("cpu").unwrap()["capabilities"] = json!(["embedding"]);
    let state = state(&gpu, Some(&cpu));
    let models = discover_models(&state).await.unwrap();
    assert!(
        models
            .iter()
            .find(|model| model.name == "gpu")
            .unwrap()
            .capabilities
            .contains(&Capability::Completion)
    );
    assert!(
        models
            .iter()
            .find(|model| model.name == "cpu")
            .unwrap()
            .capabilities
            .contains(&Capability::Embedding)
    );
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 1);
    assert_eq!(cpu.engine.show_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_tag_changed_during_show_cannot_poison_the_old_revision_cache() {
    let gpu = mock("changes", Some("revision-a")).await;
    let state = state(&gpu, None);
    assert!(discover_models(&state).await.unwrap().is_empty());
    assert!(
        state
            .catalog_metadata
            .get(&gpu.url, Some("revision-a"))
            .is_none()
    );
    expire_tags(&state).await;
    let models = discover_models(&state).await.unwrap();
    assert_eq!(models[0].digest.as_deref(), Some("revision-b"));
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pinned_request_gets_typed_identity_error_when_tag_changes_during_show() {
    let gpu = mock("changes", Some("revision-a")).await;
    let state = state(&gpu, None);
    let error = discover_for_route(&state, Some("changes"))
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::CONFLICT);
    assert_eq!(
        serde_json::to_value(&error.body).unwrap()["code"],
        "model_identity_changed"
    );
    for digest in ["revision-a", "revision-b"] {
        assert!(state.catalog_metadata.get(&gpu.url, Some(digest)).is_none());
    }
}

#[tokio::test]
async fn unconfirmed_show_serves_the_request_but_is_not_cached() {
    let gpu = mock("outage", Some("revision-a")).await;
    gpu.engine.shows.lock().await.get_mut("outage").unwrap()["capabilities"] = json!(["embedding"]);
    let state = state(&gpu, None);
    // The tags re-read fails after /show: use the fetched metadata for this request only.
    let models = discover_for_route(&state, Some("outage")).await.unwrap();
    assert_eq!(models[0].digest.as_deref(), Some("revision-a"));
    assert!(models[0].capabilities.contains(&Capability::Embedding));
    assert!(
        state
            .catalog_metadata
            .get(&gpu.url, Some("revision-a"))
            .is_none()
    );
    *gpu.engine.tags_status.lock().await = StatusCode::OK;
    discover_for_route(&state, Some("outage")).await.unwrap();
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
    // Once confirmed, the revision is cached.
    discover_for_route(&state, Some("outage")).await.unwrap();
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unconfirmed_show_never_enters_the_combined_catalog_snapshot() {
    let gpu = mock("outage-changes", Some("revision-a")).await;
    gpu.engine
        .shows
        .lock()
        .await
        .get_mut("outage-changes")
        .unwrap()["capabilities"] = json!(["embedding"]);
    let state = state(&gpu, None);
    // /show sees changed metadata, but the confirmation read is unavailable. Preserve the
    // one-request fallback without retaining that unconfirmed old-digest/new-metadata pair.
    let first = discover_models(&state).await.unwrap();
    assert_eq!(first[0].digest.as_deref(), Some("revision-a"));
    assert!(first[0].capabilities.contains(&Capability::Embedding));
    assert!(state.catalog_cache.read().await.is_none());
    assert!(
        state
            .catalog_metadata
            .get(&gpu.url, Some("revision-a"))
            .is_none()
    );

    // No TTL manipulation: the immediately following request must discover the replacement.
    *gpu.engine.tags_status.lock().await = StatusCode::OK;
    let confirmed = discover_models(&state).await.unwrap();
    assert_eq!(confirmed[0].digest.as_deref(), Some("revision-b"));
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
    assert!(state.catalog_cache.read().await.is_some());
    discover_models(&state).await.unwrap();
    assert_eq!(gpu.engine.show_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn malformed_individual_tag_and_show_do_not_hide_valid_model() {
    let gpu = mock("gpu", Some("revision-a")).await;
    gpu.engine.tags.lock().await["models"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"size":20}),
            json!({"name":"invalid-json","digest":"broken"}),
            json!({"name":"bad-shape","digest":"bad-shape"}),
        ]);
    gpu.engine
        .shows
        .lock()
        .await
        .insert("bad-shape".into(), json!({"capabilities":false}));
    let models = discover_models(&state(&gpu, None)).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "gpu");
}

#[tokio::test]
async fn pinned_primary_never_observes_failed_cpu_and_failed_pin_explains_endpoint() {
    let gpu = mock("gpu", Some("revision-a")).await;
    let cpu = mock("cpu", Some("revision-b")).await;
    cpu.engine.ps.lock().await.0 = StatusCode::SERVICE_UNAVAILABLE;
    let state = state(&gpu, Some(&cpu));
    assert_eq!(
        discover_for_route(&state, Some("gpu")).await.unwrap()[0].name,
        "gpu"
    );
    assert_eq!(cpu.engine.ps_calls.load(Ordering::SeqCst), 0);
    let error = discover_for_route(&state, Some("cpu")).await.unwrap_err();
    assert!(error.body.error.contains(&cpu.url));
    assert_eq!(discover_models(&state).await.unwrap().len(), 1);
}

#[tokio::test]
async fn untagged_pin_selects_assigned_latest_backend_and_its_revision() {
    let gpu = mock("tiny:latest", Some("primary-revision")).await;
    let cpu = mock("tiny:latest", Some("cpu-revision")).await;
    let mut state = state(&gpu, Some(&cpu));
    state.cpu_models = Arc::new(BTreeSet::from(["tiny:latest".into()]));
    let models = discover_for_route(&state, Some("tiny")).await.unwrap();
    assert_eq!(models[0].name, "tiny:latest");
    assert_eq!(models[0].digest.as_deref(), Some("cpu-revision"));
    assert_eq!(gpu.engine.ps_calls.load(Ordering::SeqCst), 0);
    discover_models(&state).await.unwrap();
    let cached = discover_for_route(&state, Some("tiny")).await.unwrap();
    assert_eq!(cached[0].digest.as_deref(), Some("cpu-revision"));
    assert_eq!(cpu.engine.show_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_live_cpu_ps_removes_cached_candidate_until_observation_recovers() {
    let gpu = mock("gpu", Some("revision-a")).await;
    let cpu = mock("cpu", Some("revision-b")).await;
    let state = state(&gpu, Some(&cpu));
    assert_eq!(discover_models(&state).await.unwrap().len(), 2);
    cpu.engine.ps.lock().await.1 = json!({"error":"not a running list"});
    let healthy = discover_models(&state).await.unwrap();
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0].name, "gpu");
    cpu.engine.ps.lock().await.1 = json!({"models":[]});
    assert_eq!(discover_models(&state).await.unwrap().len(), 2);
    assert_eq!(cpu.engine.show_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_running_observations_are_unknown_instead_of_empty() {
    assert!(running_models(json!({"error":"offline"})).is_err());
    assert!(running_models(json!({"models":[{"size":123}]})).is_err());
    assert_eq!(
        running_models(json!({"models":[]})).unwrap(),
        Vec::<Value>::new()
    );
}

#[test]
fn residency_of_a_different_revision_is_not_reused() {
    let ps = [json!({"name":"tag","digest":"old","size":100})];
    assert!(matching_resident(&ps, "tag", Some("new")).is_none());
    assert!(matching_resident(&ps, "tag", Some("old")).is_some());
}

#[test]
fn endpoint_selection_canonicalizes_latest_without_inventing_a_family_tag() {
    let assignments = BTreeSet::from(["tiny:latest".into(), "single:3b".into()]);
    assert_eq!(assigned_pin_name(&assignments, "tiny"), "tiny:latest");
    assert_eq!(assigned_pin_name(&assignments, "tiny:7b"), "tiny:7b");
    assert_eq!(assigned_pin_name(&assignments, "single"), "single");
    let names = ["tiny:latest", "single:3b", "ambiguous:3b", "ambiguous:7b"];
    assert_eq!(
        endpoint_name(&names, "tiny", true).unwrap(),
        Some("tiny:latest")
    );
    assert_eq!(
        endpoint_name(&names, "single", true).unwrap(),
        Some("single:3b")
    );
    assert_eq!(endpoint_name(&names, "single", false).unwrap(), None);
    assert!(endpoint_name(&names, "ambiguous", true).is_err());
}

#[test]
fn thinking_metadata_supersedes_coarse_capability() {
    let (caps, _) = split_capabilities(
        &json!({"capabilities":["completion"],"thinking":{"values":[false,"max"]}}),
    );
    assert!(caps.contains(&Capability::Thinking));
    let (caps, _) =
        split_capabilities(&json!({"capabilities":["thinking"],"thinking":{"values":[false]}}));
    assert!(!caps.contains(&Capability::Thinking));
}

#[test]
fn actual_catalog_constructor_only_attaches_policy_quality_to_the_measured_revision() {
    use super::super::{PlatformConfig, TaskKind, discovery::Policies};
    let config = PlatformConfig::new("127.0.0.1:0", "http://127.0.0.1:11439", None, None);
    let (_, mut state) = super::super::build(&config).unwrap();
    state.policies = Arc::new(Policies {
        qualified: BTreeMap::from([(TaskKind::Completion, vec!["tag".into()])]),
        model_digests: BTreeMap::from([(
            TaskKind::Completion,
            BTreeMap::from([("tag".into(), "measured".into())]),
        )]),
        ..Policies::default()
    });
    let entry = json!({"name":"tag","size":1,"digest":"measured"});
    let show = json!({"capabilities":["completion"]});
    assert_eq!(
        catalog_model(&state, &entry, "tag", &show, &[])
            .policy_rank
            .get(&TaskKind::Completion),
        Some(&0)
    );
    let replaced = json!({"name":"tag","size":1,"digest":"replacement"});
    assert!(
        catalog_model(&state, &replaced, "tag", &show, &[])
            .policy_rank
            .is_empty()
    );
    let undigested = json!({"name":"tag","size":1});
    assert!(
        catalog_model(&state, &undigested, "tag", &show, &[])
            .policy_rank
            .is_empty()
    );
}
