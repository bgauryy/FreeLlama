# Changelog

## Unreleased

- **Removed `benchmark/`:** the harness, hardware runners and local adapter benchmark are gone,
  and the repository no longer contains Python. Their history stays in git.
- **New `freellama policy from-eval`:** this Rust command replaces `policy_from_eval.py`. It writes
  a routing policy from a harness `aggregate.json`.
- **Model evidence moved:** `delegate_research` now reads its grades from
  `packages/mcp/evidence/model-evidence.json`. That file is not published, and
  `FREELLAMA_MCP_MODEL_EVIDENCE` still overrides it.
- **Root scripts:** `build`, `lint` and `test` now run every package (JS and Rust). `test:js`,
  `lint:js` and `build:js` run the JS side only. `test:hardware` and `test:agents:node` are removed.

## 0.6.0

Breaking cleanup release: fewer surfaces, stricter contracts, and safer admission.

### MCP server (`@octocodeai/freellama-mcp-server`)

- **9 tools:** `run_task`, `task_jobs`, `delegate_research`, `models`, `session`, `scope`,
  `ollama_manage`, `ollama_delete`, `doctor`.
  - `run_task_batch` is now `run_task {tasks:[{id, independent:true, task}]}`.
  - `warm_model` is now `run_task {warm:true, model}`.
- **`scope`:** `routeDefaults` and `limits` are removed from MCP; they remain HTTP/CLI settings
  ([Scopes and warming](docs/SCOPES_AND_WARMING.md)).
- **`delegate_research.agent`:** only `maxTurns`, `contextTokens` and `outputTokens` remain.
  - Removed: `seed`, `temperature`, `think`, `keepAlive`, `toolTimeoutSeconds`,
    `requestTimeoutSeconds`, `executionPreference`, `minPlacementEvidence`.
  - The output `execution` is now `{receipts}`.
- **`doctor`:** the `endpoint` alias is removed; use `ollamaEndpoint`. The `scheduler` and
  `config` views are folded into `full`.
- **Docs resources:** only the top-level guides are served; `freellama://docs/index` is
  `docs/README.md`. Development history (`docs/dev`) is no longer exposed.
- **Native binding:** `recommend`, `naturalRoute` and `runTask` are removed.

### CLI (`@octocodeai/freellama`)

- **Removed commands:** `natural-route`, `recommend`, `eval`, `run`, `policy-from-eval`, `tools`.
  - `policy-from-eval` is replaced by
    `python3 benchmark/harness/scripts/policy_from_eval.py`.
- **Removed flags:** `--intent-model`, `--recommendation-catalog`.
- **Removed environment variables:** use the flag or a `--runtime-config` key instead.
  - `FREELLAMA_ADAPTIVE_CONCURRENCY`, `FREELLAMA_AUTO_CONTEXT_MAX`
  - `FREELLAMA_BREAKER_COOLDOWN_SECONDS`, `FREELLAMA_BREAKER_FAILURES`
  - `FREELLAMA_CONTEXT_MODE`, `FREELLAMA_OLLAMA_DEFAULT_CONTEXT`, `FREELLAMA_PINNED_MODELS`
  - `FREELLAMA_MAX_CONCURRENT_TASKS`, `FREELLAMA_MAX_QUEUED_TASKS`
  - `FREELLAMA_CPU_MAX_CONCURRENT_TASKS`, `FREELLAMA_CPU_MAX_QUEUED_TASKS`
  - `FREELLAMA_RAW_MAX_CONCURRENT_REQUESTS`, `FREELLAMA_RAW_QUEUE_WAIT_SECONDS`
  - `FREELLAMA_RECOMMENDATION_CATALOG`, `FREELLAMA_VIEW_PORT`
  - Kept: `FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, `FREELLAMA_EVICT_IDLE_MODELS`,
    `FREELLAMA_USAGE_QUEUE_CAPACITY`, `FREELLAMA_SCOPE_*`, `FREELLAMA_KEEP_ALIVE_*`.
- **Removed HTTP endpoints:** `/natural-routes`, `/recommendations`.
- **Removed package:** the `packages/view` dashboard; use the built-in `/_freellama/ui`.

### Routing and execution

- Image input is refused unless a vision policy or a vision-capable model applies.
- OCR specialists (detected from `general.architecture` metadata) rank last for general tasks.
- Repetition loops are flagged in the result (64 KiB scan window); output is not truncated.
- `/models` reports `resident_view`.
- Unload requests no longer send `"prompt":""`.
- **Policy files:** `num_ctx` is rejected (context is routing-owned; pass `context_tokens`),
  `num_predict: 0` is rejected, and `think` requires a Thinking-capable model.

### Admission

- **Low memory:** on Linux and Windows a new load that does not fit is held, then refused with
  503 at its deadline. macOS keeps the advisory estimate (`admitted_low_memory_estimate`).
- **Resident reuse:** an already-loaded model adds no memory, so low-RAM and active-swapping holds
  no longer block it. A large load can no longer lock out its own runner, which previously made
  multi-turn `delegate_research` time out. New loads still wait while the host swaps. OS memory
  pressure, CPU load and thermal holds still apply to every request.
- **First-sample grace:** a `ResourcePolicy.first_sample_grace` field (default 2s).

### Contract changes

- The library listing reports `installed: null` with `installedUnavailable` when Ollama cannot
  be read.
- Malformed pagination cursors are rejected.
- Batch items are validated more strictly.

### Build

- Yarn now comes from `packageManager` through Corepack; the vendored Yarn binary is removed.
  Run `corepack enable` once.
- Generated logo and native README copies are no longer tracked; the build regenerates them.
- **Rust:** MSRV is 1.88 (the code needs `let` chains; the declared 1.85 never built).
  `rust-toolchain.toml` pins the compiler; CI checks the MSRV and runs `cargo deny`
  (advisories, licenses, sources).
- **Security:** `rustls` 0.23.45 fixes RUSTSEC-2026-0285.
- **Release profile:** fat LTO, one codegen unit, stripped symbols. The CLI binary shrinks from
  13.1 MB to 7.3 MB, the native addon from 7.5 MB to 4.7 MB (darwin-arm64).
- **Native binding:** napi-rs 3. `native/index.d.ts` is generated from `napi.rs` by
  `yarn build:native`, and CI fails when the committed file is stale.
- **Tests:** the Rust integration tests build as one binary (`--test integration`).
- **CLI output:** a closed pipe (`freellama doctor | head`) exits quietly with status 0 instead
  of panicking with status 101.
- **Subprocesses:** `doctor` probes, telemetry commands and the Ollama restart helper run with a
  deadline and capped output, and every child is killed and reaped. Config reloads no longer
  block a runtime worker.
