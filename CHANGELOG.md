# Changelog

## Unreleased

- **Rust crates on crates.io:** `freellama-core` (library `freellama`) and `freellama-cli` (command
  `freellama`) are publishable: `publish = false` is gone, both crates carry license texts, an
  `include` list and crates.io metadata, the CLI pins the core by version, and
  `yarn release:verify:publish` now refuses a crate that is not publishable to crates.io or lacks
  `description`, `license` or `repository`. Install with `cargo install freellama-cli`.

## 0.6.1

- **MCP server starts serve at launch:** with `FREELLAMA_MCP_AUTOSTART_SERVE` on (the default) the
  server now starts `freellama serve` in the background when it launches, not only on the first
  tool call, so other local clients of `127.0.0.1:11435` (skills, scripts, the CLI) find it in a
  fresh session instead of falling back to raw Ollama. A failed start is logged to stderr and the
  next tool call retries and reports it.
- **Docs:** `docs/dev/` (findings report, ADR, performance-testing guide and evidence notes) is
  removed; git history keeps it. The README gains a "How it compares" section with peer behavior
  from primary sources and single-machine measurements. Every guide was rechecked against the code
  (status codes, platform support, routes, flags); the CLI help for `--max-queued-tasks` now says
  429, matching the server.
- **CLI `task --input-file` works:** the positional prompt was still required, so embedding input
  from a file was refused before reaching the server. The prompt is now optional with
  `--input-file` (and conflicts with it).
- **Skill is one file:** `skills/freellama/` is now just `SKILL.md`. Its references are folded in
  or point to the docs; `scripts/check.sh` (use `freellama doctor`, `freellama status` and
  `/_freellama/v1/health`), its test, `examples/local-rag.sh` and the `test:agents` script are removed.
- **License:** copyright holder is Guy Bary; package manifests and crates name the author.
- **Test fix:** the scope and warming integration suite set `FREELLAMA_AUTOSTART_SERVE`, which no
  code reads; it now sets `FREELLAMA_MCP_AUTOSTART_SERVE=0` as intended.

## 0.6.0

Breaking cleanup release: fewer surfaces, stricter contracts, and safer admission.

### Routing, loop control and host awareness

- **Model-agnostic routing:** routing no longer ranks by a model's name or architecture. The rule
  that demoted architectures ending in `ocr` is replaced by observed evidence: automatic selection
  ranks a model last for a task kind it has looped on at least twice in at most twice as many
  managed runs (`repeats_on_this_task_last_resort`); an explicit `model` or operator policy still
  wins. `GET /_freellama/v1/models` lists the evidence as `observed_loops`.
- **No injected stop sequences:** the `glmocr`-only `options.stop` default is removed; FreeLlama
  adds no request option because of which model was chosen.
- **KV estimate from metadata:** `kv_cache_bytes_per_token_f16` is computed for any architecture
  whose metadata declares a uniform full-attention layout, instead of an allowlist of ten
  architecture names; recurrent, state-space, sliding-window, shared and cross-attention layouts
  stay `null`.
- **`stop_on_repetition` (MCP `stopOnRepetition`, CLI `--stop-on-repetition`):** streams a chat
  from Ollama and stops once the output repeats itself, keeping everything generated
  (`done_reason: "repetition"`, `output_check.stopped_early`). An Ollama "token repeat limit" abort
  now keeps the text generated before it (`done_reason: "repeat_limit"`) instead of failing.
- **Backend-aware pressure holds:** CPU-load and thermal-throttling holds now apply to CPU-assigned
  work and to cold loads, not to inference on a runner already resident on the GPU
  (`ResourceDemand::cpu_bound`); OS memory pressure still holds everything. Measured on a 12-CPU
  Mac: resident tasks waited 64 to 120 s behind a compiler's CPU load. The assessment's new
  `held_by` lists the holds that applied to the request.
- **Memory reserve capped by the load:** the headroom a new load must leave is the host reserve
  (15% / 20%) but never more than the load itself above the 1 GiB / 2 GiB floor, the way peers
  keep a fixed margin next to the load rather than a share of the host.
- **Model names resolve like Ollama:** an explicit `model` accepts a bare name (`name` is
  `name:latest`, or the one installed tag of that family); unknown and ambiguous names are
  refused with the installed candidates.
- **`GET /_freellama/v1/models`** reports each model's `context_default` (`num_ctx`, `source`),
  the context Ollama gives a request that leaves `num_ctx` unset, and `resident_context`, the loaded
  runner's context; a different explicit `num_ctx` reloads the runner.
- **Works without system CA certificates:** on a host with no CA bundle (slim or distroless
  containers) every HTTP client used to fail to build, even for plain-HTTP Ollama. FreeLlama now
  falls back to an empty trust store and warns once: `http://` upstreams work, `https://` upstreams
  fail their certificate check until `ca-certificates` is installed or `SSL_CERT_FILE` is set.
- **Heat on Apple silicon:** Apple silicon records no `pmset` thermal warning, so the thermal
  hold never fired there. FreeLlama now reads the system thermal pressure level
  (`notifyutil com.apple.system.thermalpressurelevel`); `heavy` or worse counts as throttled.
  `status` reports it as `host.thermal_pressure` and `doctor` as `host_runtime_signals.thermal_pressure`.
- **Machine and GPU identity:** `doctor`'s `machine` adds `gpu` (Apple accelerator model and core
  count from the I/O registry, or NVIDIA name and memory from `nvidia-smi` on Linux) and
  `performance_cpus` / `efficiency_cpus` on hybrid CPUs. Apple `gpu_activity` devices add
  `allocated_memory_bytes`, the driver's allocated memory.
- **Accurate `Retry-After`:** the estimate used each task's wall time including its queue wait and
  divided by budget units instead of runnable lanes, so a queued burst inflated it (76 s measured
  for a 32 s wait). It now uses service time and lanes (`budget / cost`).
- **Fixed a stale Linux-only test:** `pinned_models_are_never_evicted` expected a refusal under
  the advisory memory mode; it now checks both modes (refuse holds the load, advisory dispatches it
  with a memory advisory) and that the pinned runner is never unloaded.
- **Docs:** `ARCHITECTURE.md#request-shapes` and the MCP README give the control API's JSON
  bodies and receipt fields.
- **Fixed `build:js`:** `yarn workspaces foreach -A` included the root workspace, whose `build`
  re-ran `build:js`; the recursion forked yarn until killed. The root workspace is excluded.
- **Removed `benchmark/`:** the harness, hardware runners and local adapter benchmark are gone,
  and the repository no longer contains Python. Their history stays in git.
- **New `freellama policy from-eval`:** this Rust command replaces `policy_from_eval.py`. It writes
  a routing policy from a harness `aggregate.json`.
- **Model evidence moved:** `delegate_research` now reads its grades from
  `packages/mcp/evidence/model-evidence.json`. That file is not published, and
  `FREELLAMA_MCP_MODEL_EVIDENCE` still overrides it.
- **Root scripts:** `build`, `lint` and `test` now run every package (JS and Rust). `test:js`,
  `lint:js` and `build:js` run the JS side only. `test:hardware` and `test:agents:node` are removed.

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
  - `policy-from-eval` is replaced by `freellama policy from-eval`.
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
  pressure still holds every request.
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
  `rust-toolchain.toml` pins the compiler; `deny.toml` configures `cargo deny`
  (advisories, licenses, sources).
- **Security:** `rustls` 0.23.45 fixes RUSTSEC-2026-0285.
- **Release profile:** fat LTO, one codegen unit, stripped symbols. The CLI binary shrinks from
  13.1 MB to 7.3 MB, the native addon from 7.5 MB to 4.7 MB (darwin-arm64).
- **Native binding:** napi-rs 3. `native/index.d.ts` is generated from `napi.rs` by
  `yarn build:native`, which fails when the file declares a different number of functions than
  `napi.rs` exports.
- **Tests:** the Rust integration tests build as one binary (`--test integration`).
- **CLI output:** a closed pipe (`freellama doctor | head`) exits quietly with status 0 instead
  of panicking with status 101.
- **Subprocesses:** `doctor` probes, telemetry commands and the Ollama restart helper run with a
  deadline and capped output, and every child is killed and reaped. Config reloads no longer
  block a runtime worker.
