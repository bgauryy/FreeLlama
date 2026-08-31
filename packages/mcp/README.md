# FreeLlama MCP server

Exposes FreeLlama's local-LLM control plane, and Ollama's lifecycle, as
[MCP](https://modelcontextprotocol.io) tools, built on the official
[TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk).

**Eight tools:**

- `doctor`, `models`, `route`, `run_task` — thin wrappers over the native NAPI bindings into the
  Rust core (`../rust-core/src/napi.rs`); no CLI subprocess, no reimplemented routing logic.
- `ollama_manage`, `ollama_delete` — Ollama's HTTP API for lifecycle operations the routing layer
  doesn't cover.
- `search_models` — queries the public `ollama.com` library.
- `delegate_research` — offloads a grounded code-research question to a local model and returns a
  verdict computed from what the run did.

## Architecture

```
MCP client (Claude Desktop, etc.)
        │ stdio, JSON-RPC
        ▼
src/index.ts — registers 8 tools, forwards args, returns JSON as text
        │
        ├─ native addon (napi), for the routing-plane tools:
        │     native/index.js → native/freellama.<triple>.node (from ../rust-core/src/napi.rs)
        │     ├─ doctor()                → calls crate::doctor() directly; no serve needed
        │     ├─ machine/listModels/route → one HTTP call each to a running `freellama serve`
        │     └─ runTask()               → routes AND executes a chat/generate/embed call
        │
        ├─ direct HTTP to Ollama, for `models`' no-serve views + `ollama_manage`/`ollama_delete`
        │
        └─ subprocess, for `delegate_research`:
              spawns the research adapter (`bash` by default) against an allowlisted workspace
```

## Build

The native addon is built from the repo root; this package is built in place.

```bash
# 1. Native addon → packages/mcp/native/freellama.<triple>.node
cd <repo-root>
npm install
npm run build

# 2. This server → dist/index.js (also bundles the research adapters into adapters/)
cd packages/mcp
npm install
npm run build
```

Both steps must run once before `npm start`/`npm test`: `dist/index.js` loads `native/index.js`,
which `require()`s the compiled `.node` binary. The `.node` binary is gitignored (a build
artifact); `native/index.js` and `native/index.d.ts` are hand-written and checked in.

## Run

```bash
node dist/index.js
```

The server speaks MCP over stdio — launch it from an MCP client (for example, Claude Desktop's
`claude_desktop_config.json`), not interactively.

## Test

```bash
npm run build
npm test   # smoke-test.mjs, smoke-test-protocol.mjs, smoke-test-delegate.mjs
```

`npm test` drives the server through the real MCP protocol (the SDK's `Client` +
`StdioClientTransport`): it lists tools, calls `doctor` (works with no `freellama serve` running),
checks the tool contract, and exercises `delegate_research` against this repo.

Slower or destructive checks are run manually when touching the relevant tools:

| Script | Checks | Needs |
|---|---|---|
| `test/smoke-test-lifecycle.mjs` | pull → verify → delete round trip, net-zero | network, ~10–30s |
| `test/env-override-check.mjs` | `FREELLAMA_OLLAMA_ENDPOINT` actually changes behavior | — |
| `test/validate-all.mjs` | every tool against the live system (22 checks) | serve + Ollama, ~1m |
| `test/smoke-test-run-task.mjs` | `requiredCapabilities` filtering + real routing/execution | release binary, port 11435 free, ~15–20s |

## Tools

| Tool | Needs `freellama serve`? | What it does |
|---|---|---|
| `doctor` | Optional (machine profile needs serve) | Ollama reachability, CLI/server version match, and the 11 memory-governing settings (nine `OLLAMA_*` plus `LLAMA_ARG_FIT`/`LLAMA_ARG_FIT_TARGET`), each with its *effective* default; plus chip/memory/CPU/disk when serve is up |
| `models` | `view: "installed"` (default) does; the others don't | Four views: `installed` (capabilities, VRAM, context, policy_rank), `resident` (loaded now + GPU/CPU split), `detail` (one model, real max context), `raw` (`GET /api/tags`) |
| `route` | Yes | Deterministic model selection; accepts `requiredCapabilities` and `minConfidence` (forwarded to the core gate). Reports evidence dimensions and a `rejected[]` list |
| `search_models` | No (queries `ollama.com`) | Two-step library search: step 1 returns families, step 2 returns pullable tags with size and `fitsInMemory`. Flags `cloudOnly` models |
| `run_task` | Yes | **Routes and executes** a chat/generate/embed call. Embedding results withhold raw vectors by default (`returnEmbeddings: true` to get them) |
| `ollama_manage` | No (direct to Ollama) | `action: "pull"` downloads a model; `action: "stop"` force-unloads it. Both additive and idempotent |
| `ollama_delete` | No (direct to Ollama) | **Destructive**: permanently removes a model. Call only on an explicit human instruction naming the exact model |
| `delegate_research` | No (spawns the adapter) | Answers a grounded question from files under `workspacePath` and returns an answer, `citations[]`, and a `verification` verdict computed from what the run did — not the model's self-report |

## Configuration

Every default is overridable via an environment variable — set them in the MCP client's
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
| `FREELLAMA_MCP_ALLOWED_ROOTS` | this repo | Colon-separated allowlist of directories `delegate_research` may read |
| `FREELLAMA_MCP_MODEL_EVIDENCE` | unset | Path to per-model research grades (see below) |
| `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | `30` | Decision-only serve calls; read by `../rust-core/src/napi.rs` |
| `FREELLAMA_TASK_TIMEOUT_SECONDS` | `900` | Generation calls; read by `../rust-core/src/napi.rs` |

**Security:** `delegate_research` grants a local model read access to `workspacePath`. It is
confined to `FREELLAMA_MCP_ALLOWED_ROOTS` (default: this repo), resolved through symlinks so a link
inside an allowed root can't escape it.

**No measurements are compiled in.** Per-model research grades load at runtime from
`benchmark/evidence/model-evidence.json` (override with `FREELLAMA_MCP_MODEL_EVIDENCE`), empty by
default — an unmeasured model yields a `verify` verdict. Tool descriptions state the rules; the
figures and how they were measured live in `skills/freellama/references/` and `.octocode/evals/`.

## Platform support

The native addon is named by target triple (`freellama.<platform>-<arch>[-<abi>].node`).
`native/index.js` derives the candidate names the way napi-rs does; on an unsupported platform it
fails with a message naming exactly what it looked for and how to build it. Only the arm64 macOS
binary is currently built and shipped; `x86_64-apple-darwin` is listed in the root `package.json`'s
`napi.targets`. Other platforms need a Rust toolchain and `npm run build:native`. `engines`
requires Node >= 20.

Start the serve-backed tools' dependency from the repo root:

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

This project has never been published to a registry — treat `npm publish` as a real, irreversible,
public action and confirm with the repo owner first.

## Why native bindings

- **No reimplementation.** Every non-`doctor` tool is one HTTP call to the already-running server,
  mirroring `../cli/src/main.rs`. Routing/recommendation/discovery logic lives in exactly one place.
- **One connection pool.** `../rust-core/src/napi.rs` holds a single `reqwest::Client` in a
  `OnceLock` and clones the handle per request, matching the server side.
- **`unsafe_code` stays isolated.** The crate is `deny(unsafe_code)` except `napi.rs`, which
  carries an explicit `#[allow(unsafe_code)]` for napi-derive's FFI glue.
- **The napi build is off by default** (`cargo build` never touches it); the addon is built with
  `napi build --features napi` from the repo root.
