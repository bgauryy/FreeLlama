# FreeLlama

![A brown cartoon llama stands on a pastel cloud background.](assets/logo.jpg)

**Run your agent's bulk work on local models without overloading your machine.**

FreeLlama lets coding agents offload bounded work (embeddings, OCR and vision, bulk rewrites,
classification, summaries) to local [Ollama](https://ollama.com) models instead of spending paid
tokens and context. For each task it picks an installed model that fits, loads and switches models
only when they fit in memory, and returns the result with a receipt of where and how it ran. Your
agent still decides what to offload and verifies the answer.

## Requirements

- [Ollama](https://ollama.com/download) running (`ollama list` works) with at least one model.
  FreeLlama never downloads a model without your approval.
- Node.js 22 or newer. Prebuilt binaries for macOS arm64, Linux arm64/x64 (glibc) and Windows
  arm64/x64 ship as npm optional dependencies; keep those enabled.

## Install

### MCP server (for your agent)

Claude Code:

```bash
claude mcp add freellama -- npx -y @octocodeai/freellama-mcp-server
```

Claude Desktop, Cursor and other hosts:

```json
{ "mcpServers": { "freellama": { "command": "npx", "args": ["-y", "@octocodeai/freellama-mcp-server"] } } }
```

At launch the server starts its own FreeLlama service on `127.0.0.1:11435` if none is running, so
the CLI is optional. It does not install or start Ollama.

Required for `delegate_research` in a published install: `FREELLAMA_MCP_ALLOWED_ROOTS` lists the
directories that tool may read (`:`-separated on macOS and Linux, `;` on Windows). Unset, every local
`delegate_research` call is refused (its `adapter:"github"` runs without a workspace); the other tools
do not need it, and `run_task` `files` uses `FREELLAMA_FILE_ROOTS` instead. Add it with
`claude mcp add freellama -e FREELLAMA_MCP_ALLOWED_ROOTS=/absolute/path/to/your-project -- npx -y @octocodeai/freellama-mcp-server`
or an `"env": { ... }` entry in JSON. All settings: [MCP configuration](packages/mcp/CONFIGURATION.md).

### CLI (for the terminal)

```bash
npm install -g @octocodeai/freellama
freellama --version
```

Without installing, prefix commands with `npx @octocodeai/freellama`. Without Node.js, use
`cargo install freellama-cli` (Rust 1.89+; the command is still `freellama`).

## Quick start

### From your agent

Ask in plain words; the agent picks the tool ([MCP reference](packages/mcp/README.md)):

- "List my local models and tell me which one fits coding tasks."
- "Use FreeLlama to summarize these 40 files with a local model, then check three summaries."
- "Read this screenshot with a local OCR model."

Or call the tools directly: take the tested pick for the job from the catalog, then run it with
that `model` (without one, routing picks by capability alone and may choose a model that does
not fit now).

```jsonc
{ "tool": "models",   "arguments": { "view": "catalog", "job": "coding" } }
{ "tool": "run_task", "arguments": { "task": "coding", "model": "<catalog pick>", "prompt": "Rename foo to bar in: ...", "options": { "num_predict": 512 } } }
```

`run_task {preview:true}` shows the route without running or reserving anything.

### From the terminal

```bash
freellama init          # check prerequisites, print a first-run plan; changes nothing
freellama doctor        # Ollama reachability, versions, hardware, effective settings
freellama catalog       # tested model picks per job, what is installed, what fits now
freellama serve         # keep running: the service on http://127.0.0.1:11435
# in another terminal
freellama ready         # can work be served now, or what is it waiting for
freellama models
freellama task --task completion --model MODEL_TAG "Reply with exactly OK."
freellama status        # queues, memory holds, loaded models
```

Replace `MODEL_TAG` with a tag from `models`. `init`, `doctor` and `catalog` work without `serve`.
Every command has `--help`; the [CLI guide](docs/CLI.md) covers the rest. While `serve` runs, a
status page is at `http://127.0.0.1:11435/_freellama/ui`.

## What to offload

Offload bounded work whose result your agent can check; keep judgment in the agent.

| Offload | Keep in your agent |
|---|---|
| Embeddings, search and deduplication | Small lookups (describing the task costs more than doing it) |
| OCR and image reading | High-stakes judgment: security review, design, merge decisions |
| Bulk rewrites, summaries, classification of many files | Edits across files |
| Questions about workspace files (`delegate_research`) | Final verification of any local answer |

Many independent items: `run_task {tasks}`. Long work: `run_task {defer:true}` then `task_jobs`.
Treat local answers as candidates and spot-check some before relying on the rest.

## Safety

- Never pulls a model without approval for one exact tag and size; never deletes one unless you name
  the exact installed tag.
- Checks each load against live available memory; a load that does not fit waits, then is refused
  with a reason, and resident models keep serving.
- Unloads only models it warmed or that you name; idle models are unloaded under memory pressure.
- Holds CPU-bound work on a busy or hot machine and names the signal in the receipt.
- Binds to loopback; for remote access see [Production](docs/PRODUCTION.md).

## Documentation

| Topic | Page |
|---|---|
| All guides | [Documentation index](docs/README.md) |
| CLI commands and flags | [CLI](docs/CLI.md) |
| MCP tools, schemas, agent workflow | [MCP reference](packages/mcp/README.md), [MCP configuration](packages/mcp/CONFIGURATION.md) |
| Operating procedure for agents | [FreeLlama skill](skills/freellama/SKILL.md) |
| Features, routing and admission in depth, why FreeLlama | [Features](docs/FEATURES.md) |
| How it compares with Ollama, llama.cpp, llama-swap, LocalAI | [Comparison](docs/COMPARISON.md) |
| Choosing a model | [Model selection](docs/MODEL_SELECTION.md), [local model catalog](docs/LOCAL_MODELS.md) |
| Model forms, measured results | [Model forms and results](docs/MODEL_FORMS_AND_RESULTS.md) |
| Recipes per job | [Cookbooks](docs/cookbooks/README.md) |
| CPU/GPU setup and placement | [CPU/GPU routing](docs/CPU_GPU_ROUTING.md) |
| Queues, memory, live tuning | [Monitoring](docs/MONITORING.md) |
| Faster, lighter local models; Ollama settings | [Tuning](docs/TUNING.md) |
| Background jobs and recovery | [Deferred jobs](docs/DEFERRED_JOBS.md) |
| History, affinity, warming | [Scopes and warming](docs/SCOPES_AND_WARMING.md) |
| Deployment and releases | [Production](docs/PRODUCTION.md) |
| Build from source, operating notes | [Build and notes](docs/BUILD_AND_NOTES.md), [Testing](docs/TESTING.md) |
| Architecture and ownership | [Architecture](docs/ARCHITECTURE.md) |
| Embedding the Rust core | [Rust core](packages/rust-core/README.md) |

## License

Copyright (c) 2026 Guy Bary. Licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT),
at your option.
