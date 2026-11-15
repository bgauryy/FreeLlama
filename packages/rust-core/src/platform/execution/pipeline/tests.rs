//! Pipeline lifecycle, wait limits, memory guidance and receipt contracts.
use super::*;

mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn a_resident_task_holds_no_backend_lock_while_its_watch_runs() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut transition = Some(TransitionPermit::Resident);
        // A deferred resident request runs the same watch as a cold request. Readiness must
        // not conjure or release anything: another model can load at any time.
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn a_cold_watch_releases_the_load_lock_when_the_runner_is_ready() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let mut transition = Some(TransitionPermit::Loading(
            Arc::clone(&lock).lock_owned().await,
        ));
        assert!(lock.try_lock().is_err());
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
        assert!(lock.try_lock().is_ok(), "the next load may start");
        runner_ready(&mut transition);
        assert!(matches!(transition, Some(TransitionPermit::Resident)));
    }

    #[tokio::test]
    async fn a_cancelled_loader_releases_the_load_lock() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let transition = Some(TransitionPermit::Loading(
            Arc::clone(&lock).lock_owned().await,
        ));
        assert!(lock.try_lock().is_err());
        drop(transition); // The request was cancelled before its runner became ready.
        assert!(lock.try_lock().is_ok());
    }
}

mod wait_limit_tests {
    use super::{Duration, wait_limit};

    #[test]
    fn a_wait_lowered_by_the_operator_cap_is_reported_and_others_are_not() {
        let cap = Duration::from_secs(120);
        let limit = wait_limit(Some(1800), cap, "default").expect("1800 s exceeds the cap");
        assert_eq!(limit["requested_seconds"], 1800);
        assert_eq!(limit["effective_seconds"], 120.0);
        assert_eq!(limit["cap_source"], "default");
        assert!(
            limit["note"]
                .as_str()
                .unwrap()
                .contains("lowered the requested 1800 s")
        );
        assert!(wait_limit(Some(120), cap, "default").is_none());
        assert!(wait_limit(Some(30), cap, "env").is_none());
        assert!(wait_limit(None, cap, "default").is_none());
    }
}

mod memory_refusal_tests {
    use super::fitting_models_hint;
    use crate::platform::{CatalogModel, RouteInput};
    use serde_json::json;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn model(name: &str, capability: &str, size: u64) -> CatalogModel {
        serde_json::from_value(json!({
            "name":name,"digest":name,"size":size,"capabilities":[capability],
            "advertised_context":32768,"resident":false,"benchmark":{},"policy_rank":{}
        }))
        .unwrap()
    }

    fn route(task: &str) -> RouteInput {
        serde_json::from_value(json!({"task": task})).unwrap()
    }

    #[test]
    fn the_hint_lists_same_capability_models_that_fit_and_skips_the_rest() {
        let models = [
            model("big-decider", "decision", 9 * GIB),
            model("small-decider", "decision", GIB),
            model("chat", "completion", GIB),
        ];
        let hint = fitting_models_hint(&route("decision"), &models, 4 * GIB, GIB);
        assert!(hint.contains("`decision`"), "{hint}");
        assert!(hint.contains("small-decider (1.0 GB)"), "{hint}");
        assert!(!hint.contains("big-decider"), "{hint}");
        assert!(!hint.contains("chat"), "{hint}");
    }

    #[test]
    fn the_hint_says_none_fits_when_nothing_does() {
        let models = [model("big-decider", "decision", 9 * GIB)];
        let hint = fitting_models_hint(&route("decision"), &models, 4 * GIB, GIB);
        assert!(
            hint.starts_with("no installed `decision` model fits 4.0 GB free"),
            "{hint}"
        );
    }
}

mod sample_tests {
    use super::dedupe_sample;
    use serde_json::json;

    #[test]
    fn a_repeated_host_sample_is_kept_once_and_a_fresh_one_stays_whole() {
        let sample = json!({"available_memory_bytes": 10, "sample_age_ms": 0});
        let mut receipt = json!({
            "resource_snapshot": sample,
            "resource_admission": {"status": "admitted", "snapshot": {"available_memory_bytes": 10, "sample_age_ms": 1}},
            "resource_revalidation": {"status": "admitted", "snapshot": {"available_memory_bytes": 7, "sample_age_ms": 0}},
        });
        dedupe_sample(&mut receipt, "resource_admission");
        dedupe_sample(&mut receipt, "resource_revalidation");
        assert_eq!(
            receipt["resource_admission"]["snapshot"],
            json!({"same_sample_as": "resource_snapshot", "sample_age_ms": 1})
        );
        assert_eq!(receipt["resource_admission"]["status"], "admitted");
        assert_eq!(
            receipt["resource_revalidation"]["snapshot"]["available_memory_bytes"],
            7
        );
    }
}
