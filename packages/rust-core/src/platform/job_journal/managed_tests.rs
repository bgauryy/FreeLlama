use super::*;
use crate::platform::{Backend, TaskInput};
use crate::proxy::{LlamaCppConfig, OpenAiUpstream};

fn engine(name: &str) -> OpenAiUpstream {
    OpenAiUpstream {
        name: name.into(),
        base_url: "http://127.0.0.1:0/v1".into(),
        api_key: Some("original-secret".into()),
        llamacpp: Some(LlamaCppConfig {
            model: "text".into(),
            processor: Backend::Cpu,
            context_tokens: 4096,
            max_output_tokens: 256,
            memory_bytes: 1024,
        }),
    }
}

fn config_at(database: &Path) -> PlatformConfig {
    PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:0", None, None)
        .with_store(Store::open(database).unwrap())
        .with_durable_jobs(true)
}

fn identity(journal: &JobJournal) -> Identity {
    journal
        .identity(&None::<String>, Some("work"), "id", b"input")
        .unwrap()
}

#[test]
fn managed_identity_keeps_legacy_ollama_hashes_and_ignores_key_rotation_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir.path().join("jobs.sqlite"));
    let original = JobJournal::configured(&config).unwrap().unwrap();
    assert_eq!(
        identity(&original).input_hash,
        original.secret.hash("input", b"input")
    );
    config.upstreams = vec![engine("first"), engine("second")];
    let managed = JobJournal::configured(&config).unwrap().unwrap();
    assert_eq!(original.namespace, managed.namespace);
    assert_eq!(
        identity(&original).request_key,
        identity(&managed).request_key
    );
    assert_ne!(
        identity(&original).input_hash,
        identity(&managed).input_hash
    );
    config.upstreams.reverse();
    config.upstreams[0].api_key = Some("rotated-secret".into());
    // A passthrough-only engine does not affect managed execution.
    let mut external = engine("passthrough");
    external.llamacpp = None;
    config.upstreams.push(external);
    let reordered = JobJournal::configured(&config).unwrap().unwrap();
    assert_eq!(
        identity(&managed).input_hash,
        identity(&reordered).input_hash
    );
}

#[test]
fn every_managed_execution_setting_changes_input_identity_but_not_the_lookup_key() {
    let dir = tempfile::tempdir().unwrap();
    let original = config_at(&dir.path().join("jobs.sqlite")).with_upstreams(vec![engine("cpp")]);
    let before = JobJournal::configured(&original).unwrap().unwrap();
    for field in [
        "name",
        "url",
        "model",
        "processor",
        "context",
        "output",
        "memory",
        "removed",
    ] {
        let mut config = original.clone();
        let upstream = &mut config.upstreams[0];
        let limits = upstream.llamacpp.as_mut().unwrap();
        match field {
            "name" => upstream.name = "renamed".into(),
            "url" => upstream.base_url = "http://127.0.0.1:1/v1".into(),
            "model" => limits.model = "replacement".into(),
            "processor" => limits.processor = Backend::Gpu,
            "context" => limits.context_tokens = 8192,
            "output" => limits.max_output_tokens = 128,
            "memory" => limits.memory_bytes = 2048,
            "removed" => config.upstreams.clear(),
            _ => unreachable!(),
        }
        let after = JobJournal::configured(&config).unwrap().unwrap();
        assert_eq!(before.namespace, after.namespace, "{field}");
        assert_eq!(
            identity(&before).request_key,
            identity(&after).request_key,
            "{field}"
        );
        assert_ne!(
            identity(&before).input_hash,
            identity(&after).input_hash,
            "{field}"
        );
    }
}

#[tokio::test]
async fn engine_change_conflicts_with_retained_work_without_replaying_or_hiding_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_at(&dir.path().join("jobs.sqlite")).with_upstreams(vec![engine("cpp")]);
    let journal = JobJournal::configured(&config).unwrap().unwrap();
    let input: TaskInput = serde_json::from_value(json!({
        "model":"cpp/text", "prompt":"hello", "defer":true, "idempotency_key":"work"
    }))
    .unwrap();
    let mut serialized_input = input.clone();
    serialized_input.idempotency_key = None;
    let bytes = serde_json::to_vec(&(&serialized_input.route_fields, &serialized_input)).unwrap();
    let id = "original-job";
    let original = journal
        .identity(&input.client, Some("work"), id, &bytes)
        .unwrap();
    assert!(
        journal
            .claim(
                id.into(),
                original,
                json!({"id":id,"status":"running"}),
                crate::platform::telemetry::now_seconds() + 600
            )
            .await
            .unwrap()
            .is_none()
    );

    // Reusing the same configuration recovers the unknown outcome, without executing a worker.
    let state = crate::platform::build(&config).unwrap().1;
    let recovered = crate::platform::jobs::submit(state, input.clone())
        .await
        .unwrap()
        .0;
    assert_eq!(recovered["job"]["id"], id);
    assert_eq!(recovered["job"]["error"]["retry_safe"], false);

    config.upstreams[0]
        .llamacpp
        .as_mut()
        .unwrap()
        .max_output_tokens = 128;
    let state = crate::platform::build(&config).unwrap().1;
    let error = crate::platform::jobs::submit(state.clone(), input)
        .await
        .unwrap_err()
        .into_batch_result("retry".into());
    assert_eq!(error["code"], "task_job_idempotency_conflict");
    assert_eq!(
        crate::platform::jobs::snapshot(&state.jobs)["jobs"],
        json!([])
    );
    let changed = JobJournal::configured(&config).unwrap().unwrap();
    let retained = changed.get(id.into()).await.unwrap().unwrap();
    assert_eq!(retained["error"]["execution_outcome"], "unknown");
    assert_eq!(changed.list().await.unwrap().len(), 1);
}
