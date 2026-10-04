# Octocode research adapter

The Octocode adapter lets a local Ollama model look up repository facts through five structured
code-navigation tools. FreeLlama's `delegate_research {adapter:"octocode"}` uses this adapter for
bounded read-only lookup; the local benchmark compares it with the Bash adapter on the same suite.

The implementation and exact generated prompt live in
[`octocode_agent.py`](../scripts/octocode_agent.py). Use the
[shared adapter contract](07-adapter-contracts.md) for loop behavior and runtime defaults, and the
[MCP reference](../../../packages/mcp/README.md) for caller inputs and verification verdicts.

## Tool surface

| Tool | Purpose | Caveat |
|---|---|---|
| `localViewStructure` | Inspect a directory or file structure | Scope the path before reading content |
| `localFindFiles` | Find file names or paths | An empty filtered search does not prove absence |
| `localSearchCode` | Find text or regular-expression matches | Structural mode requires `pattern` or `rule`; `keywords` alone is invalid |
| `localGetFileContent` | Read exact file content or a bounded range | Use real search anchors and retain source evidence |
| `lspGetSemantics` | Resolve definitions, references, or symbol outlines | Empty results can reflect indexing; retry or use text search |

The model emits one JSON action per turn: `octocode`, `page`, or `finish`. `parse_action` checks the
supported tool and required action fields; the Octocode CLI validates each tool's query contract.
`safe_resolve` resolves `path` and `uri` against the workspace, rejects escapes through symlinks,
and excludes Git metadata.

## Invocation and observations

The adapter invokes `npx --yes` with the pinned `octocode@19.1.0` package, the tool name, serialized
queries, and compact output. `FREELLAMA_AGENT_OCTOCODE_PACKAGE` overrides that package deliberately.
A globally installed CLI is not required. Prepare the pinned package before a timed comparison so
first-use package download does not consume a tool deadline or distort the trial.

`ObservationStore` retains full output and serves it in pages. The model can request another stored
page without rerunning the tool. Context fitting can shorten the conversation's observation while
preserving the stored evidence. Audit results retain full tool output and invocation timing.

An exact repeated call is answered from its prior step. Invalid JSON receives a bounded repair
notice. These controls are shared with the Bash adapter; do not interpret a pre-fix run as a matched
comparison with a run using this loop.

## Transport and limits

In the benchmark, the adapter uses `FREELLAMA_OLLAMA_ENDPOINT` through the prepared serve/proxy
endpoint. In MCP delegation, `FREELLAMA_AGENT_MANAGED_ENDPOINT` routes every model turn through a
managed FreeLlama coding task with admission and placement receipts. Raw proxy calls do not acquire
the managed task contract.

The adapter exposes local read-only research tools. The caller retains task decomposition,
judgment, mutation authority, and final verification. Measure accuracy and cost for each tool surface:
the [retained comparison](../../../skills/freellama/references/task-delegation.md)
records equal pass rates with different time and token costs on one model and suite.
