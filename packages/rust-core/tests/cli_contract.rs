//! Source-level contracts for CLI flags that make advertised task kinds reachable.
use std::{fs, path::Path};

const CLI_SOURCE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/src");

/// Concatenate every file with `extension` under `dir`, recursively, in path order.
fn read_sources(dir: &Path, extension: &str) -> String {
    let mut paths = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("read source directory") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == extension) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
        .iter()
        .map(|path| fs::read_to_string(path).expect("read source file"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn cli_source() -> String {
    read_sources(Path::new(CLI_SOURCE_DIR), "rs")
}

/// `--task vision` and `--task embedding` are offered as values by clap, so the CLI must actually
/// be able to exercise them. Before `--image` existed, `--task vision` routed to a vision model and
/// then handed it nothing — an advertised capability that could not be used. These assert the
/// flags that make those task kinds reachable are still present.
#[test]
fn cli_can_actually_exercise_the_task_kinds_it_advertises() {
    let cli = cli_source();
    assert!(
        cli.contains(r#"#[arg(long = "image")]"#),
        "`--task vision` is offered but there is no --image flag to hand it an image"
    );
    assert!(
        cli.contains("input_file: Option<PathBuf>"),
        "`--task embedding` is offered but there is no --input-file flag for batch input"
    );
    assert!(
        cli.contains("general_purpose::STANDARD.encode"),
        "images must be base64-encoded before Ollama will accept them"
    );
    assert!(
        cli.contains("execution_preference: ExecutionPreference")
            && cli.contains("execution_preference: Option<ExecutionPreference>"),
        "routing uses a resolved preference; task omission preserves saved scope defaults"
    );
    assert!(
        cli.contains("cpu_max_concurrent_tasks: Option<usize>"),
        "serve must expose an independent CPU admission budget"
    );
}
