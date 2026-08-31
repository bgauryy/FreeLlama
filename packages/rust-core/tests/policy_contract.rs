//! `policy-from-eval` turns quality evidence into a routing contract. These assert it refuses to
//! manufacture evidence it does not have — the whole point of the tool.
use std::io::Write;

use freellama::policy::{qualify_from_aggregate, slug};

fn aggregate(trials: u32, pass: f64, fresh: bool) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(
        f,
        r#"{{"suite":{{"id":"s","benchmark_date":"2026-08-27","review":{{"fresh":{fresh},"review_due_at":"2026-09-27"}}}},
            "models":[{{"id":"qwen3.8-27b-mlx-bash","agent":"bash","pass_at_1":{pass},"trial_budget":{trials}}}]}}"#
    )
    .unwrap();
    f
}

#[test]
fn slug_matches_the_harness_convention() {
    assert_eq!(slug("qwen3.8:27b-mlx"), "qwen3.8-27b-mlx");
}

#[test]
fn refuses_a_smoke_run_as_evidence() {
    let f = aggregate(1, 0.9, true);
    let err =
        qualify_from_aggregate(f.path(), &["qwen3.8:27b-mlx".into()], 0.8, false).unwrap_err();
    assert!(
        err.to_string().contains("smoke result"),
        "expected a smoke-run refusal, got: {err}"
    );
}

#[test]
fn accepts_a_smoke_run_only_when_asked() {
    let f = aggregate(1, 0.9, true);
    let (q, _) = qualify_from_aggregate(f.path(), &["qwen3.8:27b-mlx".into()], 0.8, true).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].model, "qwen3.8:27b-mlx");
}

#[test]
fn refuses_an_aggregate_past_its_review_window() {
    let f = aggregate(3, 0.9, false);
    let err =
        qualify_from_aggregate(f.path(), &["qwen3.8:27b-mlx".into()], 0.8, false).unwrap_err();
    assert!(err.to_string().contains("review window"), "got: {err}");
}

#[test]
fn refuses_when_nothing_clears_the_bar() {
    let f = aggregate(3, 0.5, true);
    let err =
        qualify_from_aggregate(f.path(), &["qwen3.8:27b-mlx".into()], 0.8, false).unwrap_err();
    assert!(err.to_string().contains("cleared"), "got: {err}");
}

#[test]
fn ignores_models_that_are_not_installed() {
    let f = aggregate(3, 0.9, true);
    let err = qualify_from_aggregate(f.path(), &["some-other:7b".into()], 0.8, false).unwrap_err();
    assert!(err.to_string().contains("installed here"), "got: {err}");
}
