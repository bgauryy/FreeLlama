use super::*;
use crate::store::Store;

fn record(model: &str, ts: u64, outcome: &str) -> TaskRecord {
    TaskRecord {
        ts,
        model: model.into(),
        backend: "gpu".into(),
        task: "coding".into(),
        priority: "normal".into(),
        outcome: outcome.into(),
        status: if outcome == "ok" { 200 } else { 503 },
        prompt_tokens: 100,
        output_tokens: 20,
        duration_ms: 1500,
        queue_wait_ms: 250,
        load_ms: 0,
        output_tokens_per_second: Some(40.0),
        cached_prompt_tokens: 0,
        client: None,
    }
}

#[test]
fn civil_dates_round_trip() {
    assert_eq!(civil_day(0), "1970-01-01");
    assert_eq!(civil_day(20_725), "2026-09-29");
    for day in [0, 59, 60, 365, 10_957, 20_725, 30_000] {
        assert_eq!(day_number(&civil_day(day)), Some(day));
    }
}

#[tokio::test]
async fn usage_totals_group_by_day_and_model_and_count_errors() {
    let telemetry = Telemetry::default();
    let now = now_seconds();
    telemetry.record_task(record("a", now, "ok"));
    telemetry.record_task(record("a", now, "error"));
    telemetry.record_task(record("b", now, "ok"));
    telemetry.record_task(record("b", now - 10 * 86_400, "ok"));
    let usage = telemetry.usage(7);
    assert_eq!(usage["totals"]["tasks"], 3);
    assert_eq!(usage["totals"]["errors"], 1);
    assert_eq!(usage["by_model"]["a"]["prompt_tokens"], 200);
    assert_eq!(usage["by_day"].as_array().unwrap().len(), 1);
    assert_eq!(telemetry.usage(30)["totals"]["tasks"], 4);
}

#[tokio::test]
async fn ledger_survives_a_restart_and_skips_garbage_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("freellama.sqlite");
    let telemetry = Telemetry::new(Some(Store::open(&path).unwrap()));
    telemetry.record_task(record("a", now_seconds(), "ok"));
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    Store::open(&path)
        .unwrap()
        .append_usage(now_seconds(), "not json")
        .unwrap();
    let restarted = Telemetry::new(Some(Store::open(&path).unwrap()));
    let usage = restarted.usage(1);
    assert_eq!(usage["totals"]["tasks"], 1);
    assert_eq!(usage["ledger"]["records"], 1);
    assert_eq!(usage["ledger"]["path"], path.display().to_string());
    assert!(usage["ledger"]["last_error"].is_null());
}

#[tokio::test]
async fn bounded_queue_reports_loss_without_delaying_tasks() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let telemetry = Telemetry {
        inner: Arc::new(StdMutex::new(Inner::default())),
        ledger: Some(Store::in_memory().unwrap()),
        writer: Some(Arc::new(sender)),
        ledger_capacity: 1,
        started: Instant::now(),
    };
    for _ in 0..3 {
        telemetry.record_task(record("a", now_seconds(), "ok"));
    }
    let receipt = telemetry.ledger_receipt();
    assert_eq!(receipt["records"], 0);
    assert_eq!(receipt["pending"], 1);
    assert_eq!(receipt["dropped"], 2);
    assert_eq!(receipt["capacity"], 1);
    assert!(
        receipt["last_error"]
            .as_str()
            .unwrap()
            .contains("queue is full")
    );
    assert_eq!(telemetry.usage(1)["totals"]["tasks"], 3);
    assert!(!telemetry.flush_ledger(Duration::from_millis(10)).await);
    drop(receiver);
    telemetry.record_task(record("a", now_seconds(), "ok"));
    assert_eq!(telemetry.ledger_receipt()["dropped"], 3);
    assert!(
        telemetry.ledger_receipt()["last_error"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
}

#[tokio::test]
async fn one_worker_serializes_records_and_exits_after_final_sender_drops() {
    let store = Store::in_memory().unwrap();
    let telemetry = Telemetry::new(Some(store.clone()));
    for index in 0..100 {
        telemetry.record_task(record(&format!("model-{index}"), now_seconds(), "ok"));
    }
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    let receipt = telemetry.ledger_receipt();
    assert_eq!(receipt["records"], 100);
    assert_eq!(receipt["pending"], 0);
    assert_eq!(receipt["failed"], 0);
    assert_eq!(receipt["dropped"], 0);
    assert_eq!(replay(&store).unwrap().len(), 100);
    let sender = Arc::downgrade(telemetry.writer.as_ref().unwrap());
    let worker_state = Arc::downgrade(&telemetry.inner);
    let clone = telemetry.clone();
    drop(telemetry);
    assert!(sender.upgrade().is_some());
    drop(clone);
    assert!(
        sender.upgrade().is_none(),
        "final sink drop must close its worker queue"
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while worker_state.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("closed ledger worker must exit and release its state");
}

#[tokio::test]
async fn prometheus_text_has_typed_families_and_escaped_labels() {
    let telemetry = Telemetry::default();
    telemetry.record_task(record("we\"ird", now_seconds(), "ok"));
    telemetry.record_raw("generation", "rejected_limit");
    let text = telemetry.render_prometheus(&[
        Gauge::new("freellama_queue_depth", "Waiting tasks.", 2.0).label("backend", "gpu"),
        Gauge::new("freellama_queue_depth", "Waiting tasks.", 0.0).label("backend", "cpu"),
    ]);
    assert!(text.contains("# TYPE freellama_tasks_total counter"));
    assert!(text.contains("model=\"we\\\"ird\""));
    assert!(text.contains(
        "freellama_raw_requests_total{kind=\"generation\",outcome=\"rejected_limit\"} 1"
    ));
    assert_eq!(
        text.matches("# TYPE freellama_queue_depth gauge").count(),
        1
    );
    assert!(text.contains("freellama_queue_depth{backend=\"cpu\"} 0"));
}

/// The retry estimate's per-task time is service time: a task that waited 9 s and ran 1 s
/// counts as 1 s, so queue waits do not snowball into ever larger `Retry-After` values.
#[test]
fn average_task_time_excludes_queue_wait() {
    let telemetry = Telemetry::default();
    for _ in 0..10 {
        let mut task = record("a", now_seconds(), "ok");
        task.backend = "gpu".into();
        task.duration_ms = 10_000;
        task.queue_wait_ms = 9_000;
        telemetry.record_task(task);
    }
    assert_eq!(telemetry.average_task_ms("gpu"), Some(1_000));
}

#[tokio::test]
async fn memory_keeps_the_recent_window_and_the_view_reads_older_days_from_the_ledger() {
    let now = now_seconds();
    let old = now - u64::from(MEMORY_WINDOW_DAYS + 5) * 86_400;
    let store = Store::in_memory().unwrap();
    let telemetry = Telemetry::new(Some(store));
    telemetry.record_task(record("recent", now, "ok"));
    telemetry.record_task(record("old", old, "ok"));
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    let held = telemetry.lock().usage.keys().cloned().collect::<Vec<_>>();
    assert_eq!(held, vec![(civil_day(unix_day(now)), "recent".to_owned())]);
    // The default window and anything inside the memory window never touch the database.
    let week = telemetry.usage_report(7).await;
    assert_eq!(week["totals"]["tasks"], 1);
    assert_eq!(week["retention"]["older_days"], "not_requested");
    let longer = telemetry.usage_report(MEMORY_WINDOW_DAYS + 10).await;
    assert_eq!(longer["totals"]["tasks"], 2);
    assert_eq!(longer["by_model"]["old"]["tasks"], 1);
    assert_eq!(longer["by_day"].as_array().unwrap().len(), 2);
    assert_eq!(longer["retention"]["older_days"], "ledger");
    // A restart replays only the window, and the ledger still answers for older days.
    let restarted = Telemetry::new(telemetry.ledger.clone());
    assert_eq!(restarted.usage(REPORT_WINDOW_DAYS)["totals"]["tasks"], 1);
    assert_eq!(
        restarted.usage_report(REPORT_WINDOW_DAYS).await["totals"]["tasks"],
        2
    );

    let ephemeral = Telemetry::default();
    ephemeral.record_task(record("old", old, "ok"));
    let view = ephemeral.usage_report(MEMORY_WINDOW_DAYS + 10).await;
    assert_eq!(view["totals"]["tasks"], 0);
    assert_eq!(view["retention"]["older_days"], "not_retained");
}

#[test]
fn distinct_models_per_day_and_in_task_counters_fold_into_other_past_the_cap() {
    let telemetry = Telemetry::default();
    let now = now_seconds();
    for index in 0..MAX_KEYS_PER_DAY + 44 {
        telemetry.record_task(record(&format!("model-{index}"), now, "ok"));
    }
    // A model counted before the cap keeps its own row afterwards.
    telemetry.record_task(record("model-0", now, "ok"));
    let usage = telemetry.usage(1);
    let models = usage["by_day"][0]["models"].as_object().unwrap();
    assert_eq!(models.len(), MAX_KEYS_PER_DAY + 1);
    assert_eq!(models[OTHER_KEY]["tasks"], 44);
    assert_eq!(models["model-0"]["tasks"], 2);
    assert_eq!(usage["totals"]["tasks"], MAX_KEYS_PER_DAY + 45);
    let inner = telemetry.lock();
    assert_eq!(inner.task_models.len(), MAX_KEYS_PER_DAY + 1);
    let other = inner
        .tasks
        .iter()
        .filter(|((model, ..), _)| model == OTHER_KEY)
        .map(|(_, counters)| counters.count)
        .sum::<u64>();
    assert_eq!(other, 44);
}

#[test]
fn a_moved_window_prunes_old_days_and_earlier_client_totals() {
    let telemetry = Telemetry::default();
    let today = unix_day(now_seconds());
    {
        let mut inner = telemetry.lock();
        // A long-running process: the window last moved yesterday.
        prune(&mut inner, today - 1);
        let first_kept = window_start(today - 1, MEMORY_WINDOW_DAYS);
        inner
            .usage
            .insert((civil_day(first_kept), "a".into()), UsageTotals::default());
        inner
            .usage
            .insert((civil_day(today - 1), "a".into()), UsageTotals::default());
        inner.clients.insert((today - 1, "key".into()), 10);
    }
    let mut task = record("a", now_seconds(), "ok");
    task.client = Some("key".into());
    telemetry.record_task(task);
    let inner = telemetry.lock();
    assert_eq!(inner.retained_from, window_start(today, MEMORY_WINDOW_DAYS));
    assert_eq!(
        inner
            .usage
            .keys()
            .map(|(day, _)| day.clone())
            .collect::<Vec<_>>(),
        vec![civil_day(today - 1), civil_day(today)]
    );
    assert_eq!(
        inner.clients.keys().cloned().collect::<Vec<_>>(),
        vec![(today, "key".to_owned())]
    );
    drop(inner);
    assert_eq!(telemetry.client_tokens_today("key"), 120);
}
