use std::io::Write as _;

use super::*;

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
async fn ledger_survives_a_restart_and_skips_garbage_lines() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.jsonl");
    let telemetry = Telemetry::new(Some(path.clone()));
    telemetry.record_task(record("a", now_seconds(), "ok"));
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"not json\n")
        .unwrap();
    let restarted = Telemetry::new(Some(path));
    let usage = restarted.usage(1);
    assert_eq!(usage["totals"]["tasks"], 1);
    assert_eq!(usage["ledger"]["records"], 1);
    assert!(usage["ledger"]["last_error"].is_null());
}

#[tokio::test]
async fn bounded_queue_reports_loss_without_delaying_tasks() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let telemetry = Telemetry {
        inner: Arc::new(StdMutex::new(Inner::default())),
        ledger: Some(Arc::new(PathBuf::from("unused-test-ledger"))),
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
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.jsonl");
    let telemetry = Telemetry::new(Some(path.clone()));
    for index in 0..100 {
        telemetry.record_task(record(&format!("model-{index}"), now_seconds(), "ok"));
    }
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    let receipt = telemetry.ledger_receipt();
    assert_eq!(receipt["records"], 100);
    assert_eq!(receipt["pending"], 0);
    assert_eq!(receipt["failed"], 0);
    assert_eq!(receipt["dropped"], 0);
    assert_eq!(replay(&path).unwrap().len(), 100);
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

#[cfg(unix)]
#[tokio::test]
async fn fifo_is_rejected_on_replay_and_when_substituted_after_startup() {
    use super::ledger::append;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("usage.jsonl");
    let telemetry = Telemetry::new(Some(path.clone()));
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        replay(&path).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        append(&path, &record("a", now_seconds(), "ok"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    telemetry.record_task(record("a", now_seconds(), "ok"));
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    let receipt = telemetry.ledger_receipt();
    assert_eq!(receipt["failed"], 1);
    assert_eq!(receipt["pending"], 0);
    assert_eq!(receipt["records"], 0);
    assert!(
        receipt["last_error"]
            .as_str()
            .unwrap()
            .contains("regular file")
    );
    let restarted = Telemetry::new(Some(path.clone()));
    assert!(
        restarted.ledger_receipt()["last_error"]
            .as_str()
            .unwrap()
            .contains("regular file")
    );
    std::fs::remove_file(&path).unwrap();
    telemetry.record_task(record("a", now_seconds(), "ok"));
    assert!(telemetry.flush_ledger(Duration::from_secs(1)).await);
    let recovered = telemetry.ledger_receipt();
    assert_eq!(recovered["records"], 1);
    assert_eq!(recovered["failed"], 1);
    assert!(
        recovered["last_error"]
            .as_str()
            .unwrap()
            .contains("regular file"),
        "a successful retry must not erase earlier loss evidence"
    );
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
