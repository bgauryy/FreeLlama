use super::*;

fn event(at_ms: u64, model: &str, event: &str, duration_ms: Option<u64>) -> ResidencyEvent {
    ResidencyEvent {
        at_ms,
        backend: "gpu".into(),
        model: model.into(),
        event: event.into(),
        trigger: "managed_task".into(),
        duration_ms,
        bytes: Some(10),
    }
}

#[test]
fn two_processes_share_one_file_and_keep_the_larger_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("home").join(DATABASE_FILE);
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let row = |bytes| FootprintRow {
        backend: "gpu".into(),
        digest: "d".into(),
        context: 4096,
        num_parallel: 1,
        kv_cache_percent: 100,
        bytes,
    };
    first.record_footprints(&[row(2_000)]).unwrap();
    second.record_footprints(&[row(1_500)]).unwrap();
    assert_eq!(first.footprints(64).unwrap(), vec![row(2_000)]);
    second.put_snapshot("feedback", "{\"a\":1}").unwrap();
    assert_eq!(
        first.snapshot("feedback").unwrap().as_deref(),
        Some("{\"a\":1}")
    );
    assert_eq!(first.snapshot("missing").unwrap(), None);
}

#[test]
fn usage_and_events_read_back_by_time() {
    let store = Store::in_memory().unwrap();
    store.append_usage(100, "{\"n\":1}").unwrap();
    store.append_usage(200, "{\"n\":2}").unwrap();
    assert_eq!(store.usage_since(150).unwrap(), vec!["{\"n\":2}"]);
    store
        .record_event(&event(1_000, "a", "load", Some(800)))
        .unwrap();
    store
        .record_event(&event(2_000, "a", "load", Some(1_200)))
        .unwrap();
    store
        .record_event(&event(3_000, "a", "unload", None))
        .unwrap();
    let recent = store.events_since(1_500, 10).unwrap();
    assert_eq!(
        recent.iter().map(|event| event.at_ms).collect::<Vec<_>>(),
        vec![3_000, 2_000]
    );
    assert_eq!(
        store.load_times("gpu").unwrap(),
        vec![("a".into(), 1_000.0)]
    );
    let summary = store.event_summary(0, 1_500).unwrap();
    assert_eq!(
        summary.counts,
        vec![("load".into(), 2), ("unload".into(), 1)]
    );
    assert_eq!(summary.loads[0].measured, 2);
    assert_eq!(summary.loads[0].last_ms, 1_200);
    assert_eq!(summary.recent_loads, vec![("a".into(), 1)]);
}

#[test]
fn a_database_of_another_schema_version_is_refused_with_its_path() {
    for version in [1, 2, SCHEMA_VERSION + 1] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DATABASE_FILE);
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let message = Store::open(&path).unwrap_err().to_string();
        assert!(message.contains(&format!("schema {version}")), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("move or delete"), "{message}");
    }
}
