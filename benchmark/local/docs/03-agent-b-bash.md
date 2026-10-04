# Bash research adapter

The Bash adapter lets a local Ollama model answer repository questions through confined read-only
shell tools. It is the default adapter for FreeLlama's `delegate_research` and the baseline for the
[Octocode comparison](02-agent-a-octocode.md).

The exact prompt and task loop live in [`bash_agent.py`](../scripts/bash_agent.py). Confinement
lives in [`shell_sandbox.py`](../scripts/shell_sandbox.py); shared runtime and context behavior live
in the [adapter contract](07-adapter-contracts.md).

## Read-only tool surface

The model emits one JSON action per turn: `shell`, `page`, or `finish`. A shell action contains one
command string. The sandbox validates each command and path before executing it in the workspace.

| Control | Behavior |
|---|---|
| Tool allowlist | Read-only utilities such as `rg`, `grep`, `cat`, `find`, `jq`, bounded `sed`, and read-only Git subcommands |
| Composition | Pipes, `&&`, and `;` join validated commands |
| Writing and execution flags | Refuses output redirection, `find -exec/-delete`, `sort -o`, `rg --pre`, and Git configuration/execution overrides |
| Hidden execution | Refuses command substitution, backticks, variable expansion, loops, and subshells |
| Path confinement | Refuses paths or symlinks outside the workspace and recursive symlink-following options |
| Indirect file access | Refuses script files and filesystem-reading `xargs` targets; output-only targets are allowed |
| General executors | Does not allow `awk`, Python, or network tools |

Execution uses restricted Bash, a path containing only allowlisted tools, and a scrubbed
environment. An OS sandbox adds confinement when available: `bwrap` on Linux or `sandbox-exec` on
macOS. `FREELLAMA_AGENT_OS_SANDBOX=off` disables that OS layer while keeping command validation and
the restricted shell. Results record the selected sandbox in `model_metadata.sandbox`.

These controls apply even when MCP delegation reads the caller's real workspace. A disposable
benchmark copy is not the safety boundary. The former regex denylist is not the implemented contract.

## Observations and failures

Full command output is retained in the audit trail and served to the model in pages. A `page`
action recovers stored evidence without rerunning a command. Exact repeats return the prior step;
invalid actions receive bounded JSON repair. Context fitting preserves the system prompt and
original question by default and compacts observations before the window overflows.

A refused command consumes a turn. A successful command does not establish that the answer is
correct; the calling agent must check citations and the returned verification verdict. In MCP
use, every model turn runs as a managed coding task. Benchmark calls use the configured benchmark
serve/proxy transport. See [run flow](01-flow.md) for that distinction.

## Compare the adapters

Use the same exact model, decoding controls, context/turn budgets, fixture revisions, transport,
and grading rules. Record tool timeouts and package/index warm-up because the tool surfaces have
different overhead. Measure correctness, tokens, calls, and time separately; passing faster does
not establish better answers, and a structured tool does not imply fewer calls.

Run the confinement and action contracts through `yarn test:agents` from the repository root.
