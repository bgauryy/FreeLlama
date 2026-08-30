//! The `freellama tools` table is hand-maintained (the CLI has no Node dependency, so it cannot
//! read the MCP server's own tool list). This asserts it stays in sync with the tools the MCP
//! server actually registers, by checking the same names appear in its source.
use std::{fs, path::Path};

const MCP_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../mcp/src/index.ts");
const CLI_SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/src/main.rs");

#[test]
fn cli_tool_map_lists_every_registered_mcp_tool() {
    let mcp = fs::read_to_string(Path::new(MCP_SOURCE)).expect("read MCP source");
    let cli = fs::read_to_string(Path::new(CLI_SOURCE)).expect("read CLI source");

    let registered: Vec<String> = mcp
        .match_indices("server.registerTool(")
        .filter_map(|(i, _)| {
            let rest = &mcp[i..];
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')? + start;
            Some(rest[start..end].to_owned())
        })
        .collect();

    assert!(
        registered.len() >= 8,
        "expected at least 8 registered MCP tools, found {}: {registered:?}",
        registered.len()
    );
    for tool in &registered {
        assert!(
            cli.contains(&format!("(\"{tool}\",")),
            "`freellama tools` does not list the registered MCP tool `{tool}` — update print_tool_map()"
        );
    }
}

/// `--task vision` and `--task embedding` are offered as values by clap, so the CLI must actually
/// be able to exercise them. Before `--image` existed, `--task vision` routed to a vision model and
/// then handed it nothing — an advertised capability that could not be used. These assert the
/// flags that make those task kinds reachable are still present.
#[test]
fn cli_can_actually_exercise_the_task_kinds_it_advertises() {
    let cli = fs::read_to_string(Path::new(CLI_SOURCE)).expect("read CLI source");
    assert!(
        cli.contains(r#"#[arg(long = "image")]"#),
        "`--task vision` is offered but there is no --image flag to hand it an image"
    );
    assert!(
        cli.contains("input_file: Option<PathBuf>"),
        "`--task embedding` is offered but there is no --input-file flag for batch input"
    );
    assert!(
        cli.contains("fn base64_encode"),
        "images must be base64-encoded before Ollama will accept them"
    );
}
