//! The crate's single integration-test binary. Each `tests/*.rs` file is a module here instead of
//! its own crate, so the library links once per test build, not once per file.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and helpers unwrap; `allow-unwrap-in-tests` only covers #[test] fns"
)]

mod support;

mod adaptive_placement_contract;
mod admission_costs_contract;
mod admission_fairness_contract;
mod cli_contract;
mod context_flow_contract;
mod control_timing_contract;
mod doctor_contract;
mod edge_cases;
mod model_bench_contract;
mod monitoring_contract;
mod platform_contract;
mod proxy_contract;
mod readiness_contract;
mod repetition_stop_contract;
mod request_validation_contract;
mod resident_placement_contract;
mod resource_governor_contract;
mod scope_warming_contract;
mod scoped_multimodal_context_contract;
mod session_kill_contract;
mod smart_placement_contract;
mod structured_errors_contract;
mod task_jobs_contract;
mod warming_receipt_contract;

/// A `tests/*.rs` file missing from the list above would silently never run.
#[test]
fn every_test_file_is_declared() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let main = std::fs::read_to_string(dir.join("main.rs")).expect("tests/main.rs is readable");
    for entry in std::fs::read_dir(&dir).expect("tests/ is readable") {
        let path = entry.expect("tests/ entry is readable").path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().is_some_and(|ext| ext == "rs") && stem != "main" {
            assert!(
                main.contains(&format!("\nmod {stem};\n")),
                "tests/{stem}.rs is not declared in tests/main.rs"
            );
        }
    }
}
