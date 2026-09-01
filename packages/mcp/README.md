# FreeLlama MCP server

Exposes FreeLlama's local-LLM control plane, and Ollama's lifecycle, as
[MCP](https://modelcontextprotocol.io) tools, built on the official
[TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk).

## Understand the six tools

- `doctor`, `models`, `run_task` — thin wrappers over the native NAPI bindings into the
  Rust core (`../rust-core/src/napi.rs`); no CLI subprocess, no reimplemented routing logic.
  `run_task { preview: true }` is the free decision-only form (the former `route` tool).
  `models { view: "library" }` queries the public `ollama.com` library (the former `search_models` tool).
- `ollama_manage`, `ollama_delete` — Ollama's HTTP API for lifecycle operations the routing layer
  doesn't cover.
- `delegate_research` — offloads a grounded code-research question to a local model and returns a
  verdict computed from what the run did.

Use the smallest tool that owns the operation:

```mermaid
flowchart TD
    Q{"What does the client need?"}
    Q -->|"Diagnose the local runtime"| D["doctor"]
    Q -->|"Inspect installed, resident, detailed, raw, or online models"| M["models"]
    Q -->|"Choose or execute a task"| R["run_task"]
    Q -->|"Pull or stop a model"| O["ollama_manage"]
    Q -->|"Permanently remove a named model"| X["ollama_delete"]
    Q -->|"Ground an answer in workspace files"| G["delegate_research"]
```

| Tool | Why it exists |
|---|---|
| `doctor` | Makes runtime, version, memory, and configuration drift visible before work starts. |
| `models` | Separates installed, resident, detailed, raw, and public-library questions instead of guessing from model names. |
| `run_task` | Applies the same deterministic routing, confidence, admission, and execution contract as the Rust control plane. |
| `ollama_manage` | Exposes additive lifecycle operations that FreeLlama routing intentionally does not perform. |
| `ollama_delete` | Keeps irreversible deletion separate and explicit so a client can guard it. |
| `delegate_research` | Gives a local model bounded read-only tools and returns citations plus an independently computed verification verdict. |

## Follow the agent workflow

The server sends this workflow to every MCP client in its initialization instructions:

1. Run `doctor`. If Ollama is missing or unreachable, ask the operator to install or start it before
   selecting or downloading models.
2. Classify the task. Offload embeddings, OCR/vision, bulk transforms, and sufficiently large
   grounded lookups; retain small lookups and high-stakes judgment in the calling agent.
3. Inspect `models {view:"installed"}` and `models {view:"resident"}` so model availability and
   placement come from the current computer rather than prompt assumptions.
4. Prefer a qualified installed model and preview consequential work.
5. If none fits, collect the operator's modality, quality, context, latency, privacy, disk/download,
   and memory constraints. Search the Ollama library, inspect exact tags and host-memory fit, and
   present at most two candidates.
6. Ask the operator to approve one exact tag and its reported size before calling
   `ollama_manage {action:"pull"}`. Searching or recommending never authorizes a download.
7. Execute, inspect the route/execution/admission receipt, and verify the result at the returned
   confidence level.

The caller owns task decomposition and concurrent submission. The operator owns endpoints, exact
CPU assignments, runtime settings, and lifecycle approval. FreeLlama owns qualification, managed
placement and admission. Ollama and the OS/driver own runner loading and physical CPU/GPU execution.

## Follow the architecture

```mermaid
flowchart TD
    C["MCP client"] -->|"stdio JSON-RPC"| B["dist/index.js: six tool registrations"]
    B --> N["native/index.js and platform addon"]
    N -->|"doctor"| L["Rust diagnostics"]
    N -->|"models installed/resident; run_task"| S["freellama serve"]
    S --> G["Primary Ollama"]
    S --> U["Optional CPU Ollama"]
    B -->|"detail, raw; manage; delete"| G
    B -->|"library view"| W["ollama.com library"]
    B -->|"delegate_research"| A["allowlisted local adapter subprocess"]
    A --> S
```

`doctor`, the installed/resident model views, and `run_task` use the NAPI binding instead of a CLI
subprocess. Direct lifecycle and no-serve model views use Ollama HTTP. `delegate_research` starts a
bounded adapter against an allowlisted workspace; each adapter model turn re-enters managed
`run_task` as `coding`, so it shares routing, admission, and placement evidence. Routing logic is
not duplicated in TypeScript.

## Build

Build the native addon from the repository root and this package in place.

```bash
# From the repository root (yarn workspaces — one install for every package)
yarn install
yarn build
```

`yarn build` writes **one** MCP file, `packages/mcp/dist/index.js` (shebang, executable). The native
addon cannot go in a JS bundle; it stays at `native/freellama.<triple>.node`. Research adapters are
copied into `adapters/` for packed installs.

Piecewise, if you are iterating on one layer:

```bash
# 1. Native addon → packages/mcp/native/freellama.<triple>.node
yarn build:native

# 2. This server → dist/index.js via esbuild (then copies the research adapters into adapters/)
yarn workspace freellama-mcp-server build
```

`yarn start` needs `dist/index.js` and the compiled `.node` addon — `yarn build` from the repository
root produces both. `dist/index.js` loads `native/index.js`, which `require()`s the `.node` binary.
The `.node` binary is gitignored (a build artifact); `native/index.js` and `native/index.d.ts` are
hand-written and checked in.

## Run

```bash
node dist/index.js
```

The server speaks MCP over stdio — launch it from an MCP client (for example, Claude Desktop's
`claude_desktop_config.json`), not interactively.

## Test

Vitest, in three tiers (all TypeScript, under `test/`):

| Tier | Command (from this package) | Checks | Needs |
|---|---|---|---|
| `test/unit/` | `yarn test` (watch: `yarn test:watch`) | pure functions straight from `src/*.ts` — no build step, this is the TDD loop | nothing, ~0.5s |
| `test/integration/` | `yarn test:integration` | the built server over the real MCP protocol: tool contract, structured content, guardrails; rebuilds `dist/` itself | live Ollama on :11434, ~5s |
| `test/e2e/` | `yarn test:e2e` | every tool against the live system: real routing/execution, pull → delete round trip (net-zero), a real delegated research run | serve + Ollama + models, ~40s |

The same commands work from the repository root (`yarn test` there also runs the CLI package's tests).
The integration/e2e tiers fail fast with a readable message when Ollama is down, and the lifecycle
test refuses to delete a model already installed on the test system.

## Tools

| Tool | Needs `freellama serve`? | What it does |
|---|---|---|
| `doctor` | No | Ollama reachability, CLI/server version match, and the 11 memory-governing settings (nine `OLLAMA_*` plus `LLAMA_ARG_FIT`/`LLAMA_ARG_FIT_TARGET`), each with its *effective* default; local chip/RAM from macOS `sysctl`, Linux `/proc`, or Windows system APIs even when serve is down. Serve's profile is preferred when it is up. |
| `models` | `installed` (default) and `resident` do; `detail`/`raw`/`library` don't | Views: `installed` (capabilities, derived `model_type`, VRAM, context, policy_rank), `resident` (loaded now + managed GPU/CPU split), `detail` (one model, real max context), `raw` (`GET /api/tags`), `library` (two-step ollama.com search: omit `model` for families, pass `model:"<family>"` for tags/`fitsInMemory`) |
| `run_task` | Yes | **Routes and executes** a chat/generate/embed call. `preview: true` returns the route decision only. Embedding results withhold raw vectors by default (`returnEmbeddings: true` to get them) |
| `ollama_manage` | No (direct to Ollama) | `action: "pull"` downloads a model; `action: "stop"` force-unloads it. Both additive and idempotent |
| `ollama_delete` | No (direct to Ollama) | **Destructive**: permanently removes a model. Call only on an explicit human instruction naming the exact model |
| `delegate_research` | Yes (and spawns the adapter) | Answers a grounded question from files under `workspacePath`; managed coding turns return placement receipts, citations, and an independent verification verdict |

### Preserve Ollama request controls

`run_task.messages` preserves Ollama message fields beyond `role` and `content`, including images,
thinking, tool calls, and tool names. Managed requests also accept `format`, `think`, `options`,
`logprobs`, and `topLogprobs`. Use `contextTokens` for `num_ctx`; placement owns `num_gpu`, so both
keys are rejected inside `options`. The raw proxy remains available when a caller needs an Ollama
endpoint or feature that the managed MCP tool does not expose, including streaming.

`model_type` is a display-oriented value derived from Ollama's additive capabilities:
`generative`, `multimodal`, `embedding_only`, or `unknown`. Routing continues to use the original
capability set, not this summary label. Unknown future Ollama capabilities are omitted from the
typed routing set instead of being treated as a known capability.

### Use CPU assignments through MCP

`run_task` delegates to `freellama serve`, so it honors the server's `--cpu-upstream` and
`--cpu-model` configuration. Exact assigned models can execute on the CPU process while other
managed tasks execute on the primary GPU-capable process.

Set `executionPreference` on `run_task` to `auto` (default), `prefer_cpu`, or `prefer_gpu`. This is a
guarded hint: only exact operator-assigned CPU models are eligible, explicit model/session pins win,
and `preview: true` returns `execution.preference_satisfied`, placement, upstream, admission, and a
reason without generating. Automatic feedback normalizes work by tokens, waits for three successful
warm, physically verified samples per task on both backends, requires a 10% advantage, and never
steers the `quality` objective. `minPlacementEvidence:"observed"` fails closed unless resident
`/api/ps` evidence matches the configured processor; warm cold models once with `"configured"`.
`keepAlive:"0"` uses an observe-then-unload transaction: FreeLlama retains the runner long enough
to inspect `/api/ps`, accepts feedback only for verified placement, requests an explicit unload,
then reports the unload verification in `execution.lifecycle`.

`models {view: "resident"}` uses the managed catalog, so it includes resident models from both
Ollama processes and labels explicitly assigned CPU models instead of querying only the primary
server.

Before relying on that placement, query `/_freellama/v1/health` and require
`contracts.placement_observation:"ollama_api_ps_after_execution"` and
`contracts.placement_evidence_gate:"configured_or_observed"`. A missing contract means the running server
predates the dual-backend build even if the MCP package itself is current.

The direct Ollama tools do not infer that assignment. `ollama_manage`, `ollama_delete`, and the
no-serve model views target their explicit `ollamaEndpoint` or `FREELLAMA_OLLAMA_ENDPOINT`, which
defaults to the primary Ollama server. Point them at the CPU endpoint only when you intend to
inspect or modify that separate server. Read
[CPU and GPU model routing](../../docs/CPU_GPU_ROUTING.md) for the complete contract.

## Configuration

Every default is overridable through an environment variable — set them in the MCP client's
server-launch config (for example `.mcp.json`'s `env` block) or the launching shell.

| Variable | Default | Affects |
|---|---|---|
| `FREELLAMA_OLLAMA_ENDPOINT` | `http://127.0.0.1:11434` | `doctor` and all `ollama_*` tools' default Ollama endpoint |
| `FREELLAMA_SERVE_ENDPOINT` | `http://127.0.0.1:11435` | Default `endpoint` for serve-backed tools and `delegate_research` |
| `FREELLAMA_MCP_DEFAULT_MODEL` | `qwen3.8:27b-mlx` | `delegate_research`'s default `model` |
| `FREELLAMA_MCP_DEFAULT_ADAPTER` | `bash` | `delegate_research`'s adapter (`octocode` to switch) |
| `FREELLAMA_MCP_MAX_TURNS` | `8` | Max agent turns for `delegate_research` |
| `FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS` | `180` | Whole-subprocess timeout for `delegate_research` |
| `FREELLAMA_MCP_PULL_TIMEOUT_SECONDS` | `1200` | Default `ollama_manage` `"pull"` timeout |
| `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS` | `30` | Timeout for other direct Ollama HTTP calls |
| `FREELLAMA_MCP_ALLOWED_ROOTS` | checkout: this repository; published: must be set | Colon-separated allowlist of directories `delegate_research` may read on macOS and Linux; see the Windows limitation after this table |
| `FREELLAMA_MCP_MODEL_EVIDENCE` | checkout: `benchmark/evidence/model-evidence.json`; published: unset | Path to per-model research grades (see "Runtime evidence") |
| `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | `30` | Decision-only serve calls; read by `../rust-core/src/napi.rs` |
| `FREELLAMA_TASK_TIMEOUT_SECONDS` | `900` | Generation calls; read by `../rust-core/src/napi.rs` |
| `FREELLAMA_AUTH_TOKEN_FILE` | unset | Bearer-token file used by the native client and research adapters for all serve routes |
| `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR` | platform data directory | Prompt-free, model-specific token-estimator calibration shared across adapter processes |

Research-adapter settings are deployment defaults. The optional `delegate_research.agent` object
exposes the same values per call using camelCase; explicit per-call fields win.

The allowed-root parser uses `:` as its list separator. That works for macOS and Linux but
conflicts with Windows drive-letter paths such as `C:\\code`. Until the parser uses the platform
path separator, run `delegate_research` from WSL or treat that tool as unsupported on native
Windows. The remaining MCP tools do not use this file allowlist.

| Variable | Default | Affects |
|---|---:|---|
| `FREELLAMA_AGENT_NUM_CTX` | `8192` | Ollama context window |
| `FREELLAMA_AGENT_EXECUTION_PREFERENCE` | `auto` | Managed coding-agent backend hint |
| `FREELLAMA_AGENT_MIN_PLACEMENT_EVIDENCE` | `configured` | Set `observed` to fail closed on cold/mismatched placement |
| `FREELLAMA_AGENT_NUM_PREDICT` | `512` | Reserved output budget |
| `FREELLAMA_AGENT_TEMPERATURE` / `FREELLAMA_AGENT_SEED` | `0` / `42` | Repeatable decoding |
| `FREELLAMA_AGENT_THINK` / `FREELLAMA_AGENT_KEEP_ALIVE` | `false` / `5m` | Ollama reasoning and residency |
| `FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS` | `600` | Each Ollama chat attempt |
| `FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS` | Bash `30`, Octocode `45` | Each local tool call |
| `FREELLAMA_AGENT_RETRY_ATTEMPTS` / `FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS` | `2` / `5` | Conversation-level retry |
| `FREELLAMA_AGENT_MAX_PARSE_REPAIRS` / `FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS` | `2` / `500` | Invalid JSON correction limit and retained reply text |
| `FREELLAMA_AGENT_CHARS_PER_TOKEN` | `4` | First-call token estimate; later calibrated from Ollama |
| `FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS` | `256` | Input headroom beyond `num_predict` |
| `FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE` | `1024` | Bounded charge per image |
| `FREELLAMA_AGENT_KEEP_RECENT` | `2` | Recent observations preserved verbatim |
| `FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS` | `180` | Old-observation breadcrumb size |
| `FREELLAMA_AGENT_COMPACT_RETAIN_RATIO` | `0.8` | Current payload retained per emergency pass |
| `FREELLAMA_AGENT_CLIP_HEAD_RATIO` | `0.667` | Head share when an emergency clip preserves both ends |
| `FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS` | `3000` | Lossless observation page target |
| `FREELLAMA_AGENT_PINNED_OVERFLOW` | `error` | Preserve system/task bytes; `clip` is explicit opt-in |

Invalid values fail before an Ollama request. Each successful result returns
`contextManagement` with the resolved policy, calibration source/scale, estimated input budget,
and compaction count. Ollama has no stable preflight tokenizer endpoint, so “exact before the first
call” is not claimed. Model-specific calibration persists after successful calls, so later
processes start conservatively from prior `prompt_eval_count` evidence without sharing estimates
between model templates.

**Security:** `delegate_research` grants a local model read access to `workspacePath`. The path is
confined to `FREELLAMA_MCP_ALLOWED_ROOTS` (default: this repository in a checkout; unset in a published
install until you set it), resolved through symlinks so a link inside an allowed root can't escape
it. The default `bash` adapter also rejects home-directory paths, `..`, and absolute paths outside
that workspace. Production deployments should generate a permission-restricted token with
`freellama auth-token`, start `serve` with `--auth-token-file`, and set
`FREELLAMA_AUTH_TOKEN_FILE` for MCP. Authentication covers managed and raw passthrough routes; use
an external TLS and authorization layer for untrusted or multi-tenant networks.

### Runtime evidence

**No measurements are compiled in.** In a repository checkout, per-model research grades load from
`benchmark/evidence/model-evidence.json`. A published package does not include that repository
file, so its evidence table is empty unless you set `FREELLAMA_MCP_MODEL_EVIDENCE`. An unmeasured
model yields a `verify` verdict. Tool descriptions state the rules; measurement details live in
`skills/freellama/references/` and `.octocode/evals/`.

## Platform support

The native addon is named by target triple (`freellama.<platform>-<arch>[-<abi>].node`).
`native/index.js` derives candidate names the way napi-rs does; on an unsupported platform it
reports exactly what it looked for and how to build it. The target manifest covers macOS arm64/x64,
Linux arm64/x64 with GNU or musl, and Windows arm64/x64 with MSVC. A given package release can still
contain fewer prebuilt artifacts; missing targets require a Rust toolchain and `yarn build:native`.
`engines` requires Node >= 20.

Machine discovery distinguishes total host memory (`memory_bytes`) from known unified memory
(`unified_memory_bytes`). Library-tag `fitsInMemory` is a conservative host-memory preflight and
includes `fitScope: "host_memory_budget_only"`; inspect `/api/ps` for actual accelerator placement.

Start the serve-backed tools' dependency from the repository root:

```bash
cargo run --release -- serve --recommendation-catalog recommendations.example.toml
```

See `skills/freellama/references/proxy-vs-serve.md` for the `proxy` vs `serve` distinction (only
`serve` has the `/_freellama/v1/*` routes these tools call).

## Publish

`packages/mcp/` is self-contained: the native addon lives inside it, so it can be packed and
published on its own. `package.json`'s `files` field (`dist`, `native`, `adapters`, `README.md`) is
the allowlist `npm pack`/`npm publish` uses — it ships the compiled `.node` binary even though
`*.node` is gitignored. `prepublishOnly` rebuilds both the addon and `dist/` so a publish can never
ship a stale artifact.

```bash
cd packages/mcp
npm pack --dry-run   # confirm exactly what a publish would ship
```

Version 0.2.0 is not published to a registry. Treat `npm publish` as a real, irreversible public
action, and confirm with the repository owner first.

## Why native bindings

- **No reimplementation.** `run_task`, `route`/`preview`, and the installed-model list are HTTP
  calls into a running `freellama serve`, matching `../cli/src/main.rs`. `doctor` talks to Ollama
  and the host's portable machine-discovery layer directly. `ollama_manage` / `ollama_delete` hit Ollama's HTTP API. `delegate_research`
  is a subprocess. Routing logic still lives in exactly one place — the Rust core.
- **One connection pool.** `../rust-core/src/napi.rs` holds a single `reqwest::Client` in a
  `OnceLock` and clones the handle per request, matching the server side.
- **`unsafe_code` stays isolated.** The crate is `deny(unsafe_code)` except `napi.rs`, which
  carries an explicit `#[allow(unsafe_code)]` for napi-derive's FFI glue.
- **The napi build is off by default** (`cargo build` never touches it); the addon is built with
  `napi build --features napi` from the repo root.
