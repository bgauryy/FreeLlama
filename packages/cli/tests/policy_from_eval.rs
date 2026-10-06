//! `freellama policy from-eval` end to end: the binary lists installed tags from a mock Ollama
//! `/api/tags`, writes the policy, and reports failures as one `error:` line with exit status 1.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and helpers unwrap; `allow-unwrap-in-tests` only covers #[test] fns"
)]

use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    thread,
};

const INSTALLED: &str = "qwen3.8:27b-mlx";

/// Serve `/api/tags` listing `tags` (404 for any other path) until the test process exits.
fn mock_ollama(tags: &[&str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let models: Vec<String> = tags
        .iter()
        .map(|tag| format!(r#"{{"name":"{tag}"}}"#))
        .collect();
    let body = format!(r#"{{"models":[{}]}}"#, models.join(","));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let status = if request.starts_with(b"GET /api/tags ") {
                "200 OK"
            } else {
                "404 Not Found"
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    format!("http://{address}")
}

fn aggregate(trials: u32, passed: f64, fresh: bool) -> String {
    format!(
        r#"{{"suite": {{"id": "s", "benchmark_date": "2026-08-27", "review": {{"fresh": {fresh}, "review_due_at": "2026-09-27"}}}},
           "models": [{{"id": "qwen3.8-27b-mlx-bash", "agent": "bash", "pass_at_1": {passed}, "trial_budget": {trials}}}]}}"#
    )
}

fn from_eval(aggregate: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_freellama"))
        .args(["policy", "from-eval", "--aggregate"])
        .arg(aggregate)
        .args(extra)
        .env_remove("FREELLAMA_OLLAMA_ENDPOINT")
        .output()
        .unwrap()
}

#[test]
fn lists_installed_tags_and_writes_the_policy() {
    let endpoint = mock_ollama(&[INSTALLED]);
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aggregate.json");
    std::fs::write(&source, aggregate(3, 0.9, true)).unwrap();
    let out = dir.path().join("platform.toml");
    let output = from_eval(
        &source,
        &[
            "--task",
            "code-repair",
            "--out",
            out.to_str().unwrap(),
            "--ollama-endpoint",
            &endpoint,
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("wrote {}\n", out.display())
    );
    let parsed: toml::Table = std::fs::read_to_string(&out).unwrap().parse().unwrap();
    let models: Vec<&str> = parsed["policies"]["code_repair"]["qualified_models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model.as_str().unwrap())
        .collect();
    assert_eq!(models, [INSTALLED]);
}

#[test]
fn writes_the_policy_to_stdout_without_out() {
    let endpoint = mock_ollama(&[INSTALLED]);
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aggregate.json");
    std::fs::write(&source, aggregate(1, 0.9, true)).unwrap();
    let output = from_eval(
        &source,
        &[
            "--task",
            "coding",
            "--allow-smoke",
            "--ollama-endpoint",
            &endpoint,
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("schema_version = 1\nsmoke = true\n\n"),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("# source        = {}\n", source.display())));
    assert!(stdout.contains("# qwen3.8:27b-mlx — pass_at_1 0.900 over 1 trial(s), agent bash\n"));
}

#[test]
fn a_malformed_aggregate_reports_a_parse_error_not_a_traceback() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aggregate.json");
    std::fs::write(&source, "{ not json at all").unwrap();
    let output = from_eval(
        &source,
        &[
            "--task",
            "coding",
            "--ollama-endpoint",
            "http://127.0.0.1:9",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: parse aggregate "), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[test]
fn a_smoke_run_is_refused_without_allow_smoke() {
    let endpoint = mock_ollama(&[INSTALLED]);
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aggregate.json");
    std::fs::write(&source, aggregate(1, 0.9, true)).unwrap();
    let output = from_eval(
        &source,
        &["--task", "coding", "--ollama-endpoint", &endpoint],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("error: `qwen3.8-27b-mlx-bash` cleared 0.80"),
        "{stderr}"
    );
    assert_eq!(output.stdout, b"");
}

#[test]
fn an_unknown_task_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("aggregate.json");
    let output = from_eval(&source, &["--task", "poetry"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("unknown task 'poetry'; choose one of completion, coding, code_repair"),
        "{stderr}"
    );
}
