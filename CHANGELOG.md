# Changelog

All notable changes to FreeLlama are recorded here. Versions follow [Semantic Versioning](https://semver.org/);
every npm package (`@octocodeai/freellama`, `@octocodeai/freellama-mcp-server`, the native platform
packages) and the Rust crates share one version, set with `yarn release:version <semver>`.

## [Unreleased]

### Added

- **End-to-end Ollama review.** A dated [architecture and feature review](docs/OLLAMA_REVIEW.md)
  compares product directions, checks primary Ollama sources, records concrete repairs and
  separates deterministic contracts from retained model results and unobserved hardware lanes.
- **Retained coder command audits.** Private raw logs and per-command metadata survive isolated
  copy cleanup, including no-patch and interrupted runs. Bounded model previews reference the
  full audit; a byte cap or write failure terminates the command and marks output incomplete.
  [Limits and retention](docs/cookbooks/research-agent.md).
- **Managed llama.cpp text engine.** An operator-configured loopback server can run explicit
  `name/model` completion, coding and long-context tasks through the Rust admission pools and
  resource governor, with bounded context, output, response size and time. Its own server owns
  model loading; physical placement remains unknown. Tools, vision, embeddings, streaming and
  Ollama lifecycle controls are not supported on this path. An ambiguous inference failure is
  never replayed. [Setup and retained live smoke](docs/LLAMACPP.md).
- **Machine-local calibration.** `bench-all` schema 2 records host and hardware provenance;
  routing uses throughput evidence only when the local endpoint and model artifact also match.
  Job selection asks the Rust router first, requiring quality evidence for capable workflows
  and OCR, then falls back to the shipped catalog and capability selection. Explicit models
  still win; speed does not qualify task quality. [Selection rules](docs/MODEL_SELECTION.md).
- **Reproducible reliability evidence.** A 30-case labeled corpus measures `check:` misses and
  false alarms. A public tool-calling harness retains prompts, settings, digests, trials and
  deterministic grading; the first two seven-case model smoke runs score 6/7 and 5/7. Other
  cataloged audit tags remain unmeasured. Tool-call checks validate the supported schema subset
  and report unsupported constraints as unchecked. These small public suites do not establish
  general model reliability. [Reports and limits](docs/RELIABILITY.md).
- **Cross-platform contract CI.** GitHub Actions is configured for Linux, macOS and Windows,
  including Windows host-memory observation, Rust lint on Linux and packaging fixtures on
  POSIX. Local checks passed on macOS; hosted CI and the complete production release gate have
  not been observed for this change. [Validation scope](docs/TESTING.md).
- **Current project assessment.** A dated [readiness scorecard](docs/ASSESSMENT.md) rates each
  implemented priority and the remaining gaps separately from historical model benchmarks.

- **Streamed managed tasks.** `POST /_freellama/v1/tasks` with `"stream": true` answers with
  server-sent events: `route`, `admission` phases, a `delta` per generated piece, then `done` with
  the same receipt a non-streaming call returns (or `error` with `retry_safe`). Fallback, admission
  and the memory gate are unchanged; a client that disconnects releases local execution permits
  and closes the upstream request to request cancellation. Only
  generating tasks stream; anything else is refused with 422 `stream_unsupported`.
- **Token progress for MCP `run_task`.** A deferred chat job counts its tokens, and MCP progress
  reads "generating on MODEL: N tokens, R tok/s" instead of a bare "running".
- **Where the time went.** Every managed receipt has `timing`: milliseconds per phase inside
  `serve` (`discover_ms`, `prepare_ms`, `admission_ms`, `upstream_ms`, `placement_ms`,
  `learning_ms`, `persist_ms`…) and `overhead_ms`, everything except Ollama's own answer.
- **Benchmark harness.** macOS available memory (the core's estimate), process RSS and `size_vram`
  per block; the model unloaded between Ollama-backed blocks; a load gate
  (`--max-load-per-cpu`); `--concurrency 2,4,8`; `--cold`; `--hog-gb` with `memory-hog.mjs`;
  the `freellama-stream` protocol with client first-token time; pooled reports from repeated
  `--report`; preflight reports owned-start vendors and checks the manifest's weight layer.

- **One-line install.** `curl -fsSL https://raw.githubusercontent.com/bgauryy/FreeLlama/main/scripts/install.sh | sh`
  on macOS and Linux, or `irm …/scripts/install.ps1 | iex` on Windows, installs FreeLlama: the npm
  package when npm is on PATH, otherwise the checksum-verified binary of the latest release. When
  Ollama is missing, the script asks before running Ollama's official installer
  (`--with-ollama` / `--no-ollama` decide up front) and never downloads a model. A pinned
  `--version` still installs the verified binary, as before. The "Ollama is not installed" message
  from the CLI, the MCP server and `freellama init` now names the same one-line command. Ollama is
  not bundled: its own download is 177 MB on macOS and about 1.5 GB on Linux and Windows, and a
  second copy would compete with an existing install for port 11434.

### Changed

- **Bounded response handling.** Managed Ollama output and transformed/collected agent CLI and
  gateway responses now enforce the configured wire-byte limit, including error bodies and
  malformed partial records. Internal catalog and proxy metadata share a 4 MiB reader. Managed
  streams use bounded channels and retain their execution deadline under slow readers. These
  are per-response limits, not a global heap budget. [Resource boundaries](docs/MONITORING.md).
- **Agent protocol correctness.** Direct CLI/gateway calls carry configured operator auth,
  refuse redirects and off-origin forwarding, verify HTTPS certificates and honor managed
  controls. Terminal streams fail on missing completion evidence while preserving partial
  output; uncertain inference is not replayed. Windows launcher and installer fixtures now
  cover their platform-specific paths. [Agent CLI](docs/AGENT_CLI.md).
- **Durable managed-engine identity.** Changing a configured llama.cpp execution mapping
  conflicts with a retained idempotency key while preserving access to the original receipt.
  Key rotation and declaration order do not invalidate identity. [Migration](docs/DEFERRED_JOBS.md#idempotency).
- **Checks and evidence integrity.** Numeric-token matching and malformed tool-call warnings
  close specific missed checks. Reliability reports verify captured requests, reject duplicate
  acquisitions and count failed continuations as failures. New versioned reports preserve the
  original raw trials and strict scores. [Evidence and limitations](docs/RELIABILITY.md).
- **Single source for Octocode instructions.** Build-time Markdown imports replace drifting
  embedded copies; existing equality and size checks continue to enforce the compact contract.
- **Windows memory admission.** Bounded CIM sampling supplies checked total/free physical
  memory, and Windows now defaults to requiring memory telemetry. Missing or malformed values
  remain unknown; CPU load, pressure, swap and thermal signals are still unavailable. Fixture
  coverage is local; native Windows execution awaits the configured CI check.
- **Benchmark ownership and defaults.** `bench-all` writes to the FreeLlama home by default,
  validates exact `--include` tags before generation, preserves pre-existing residents and
  cleans up only eligible models it benchmarked. It calls Ollama directly, so the operator must
  choose a quiet machine and bounded inventory.
- **Focused Rust and handler refactoring.** Split benchmark provenance, pipeline helpers,
  several large test modules and MCP result construction from their owning files. The targeted
  panic/lock audit found no production `unwrap_used` or `await_holding_lock` lint hits; 30
  explicit `expect` sites remain a documented review boundary. Poison, sampler panic and
  subprocess cleanup tests cover selected failures, not a proof of all lock ordering.
  [Audit](docs/RUST_RESOURCE_AUDIT.md).
- **Documentation reconciled with implementation.** Guides and agent instructions distinguish
  the llama.cpp text subset, machine-local selection, Windows telemetry, caller verification,
  published prebuilt installs and source prerequisites. The historical 98% workflow result is
  qualified as 45/46 completed cases (45/48 nominally planned), against 25/48 for the baseline.

- **Managed-path overhead down about 6×.** Profiling with the new `timing` field found three costs
  on every request: an `/api/tags` read for a pinned model (Ollama reads every manifest, 13–43 ms),
  a host-sampling wait every 2 s (up to ~55 ms), and the feedback database write (5–11 ms). Pinned
  requests now reuse an endpoint's tag list for 10 s (read again at once for a tag it does not
  name), host samples older than 2 s are refreshed in the background (a reader still waits past
  6 s or after a load), the Ollama settings probe runs in the background, and learned state is
  written after the response, coalesced. Same host and load: median overhead 22.9 → 3.7 ms, p90
  60.3 → 5.6 ms.
- **The pressure sweep keeps runners other clients use.** Under swapping or OS memory pressure the
  sweep released any runner with no FreeLlama task in flight, so it unloaded a model another client
  used every 400 ms (seen in the 2026-11-14 benchmark). It now treats an Ollama `expires_at` that
  moved since its last check as recent use (`used_elsewhere` in the audit), and keeps a 15 s idle
  floor (`warming.min_seconds`) under pressure instead of none.

- **Agent-facing MCP text, checked by interview.** The server instructions and tool descriptions
  now answer gaps that two or more of three Claude Haiku interviews raised, each fact checked
  against the code:
  - send one of `workflow`, `job` or `task` (a conflict is refused), and `workflow` and `job` apply
    to the whole batch;
  - where `session_id`, `job_id` and the next `scopeRevision` come from, and that a fork returns a
    new `scope_id` at revision 0;
  - a warmed prompt cache is reused only with the same model, `contextTokens` and an identical
    prefix;
  - `doctor` views, `models` views and the library `model` field;
  - the `delegate_research` timeout formula, with the coder's `diff` and `applyCommand` now in its
    output schema;
  - `audio` points to the speech-to-text catalog job;
  - sweep: globs, the meaning of "unsure", and that clear chunks are not proven clean;
  - a decision probability's coin-flip band;
  - finding related files uses embedding + `nearest`, not sweep.

  Over three interviews each, the 18-scenario score rose from 52/54 to 54/54, mean tool confidence
  from 7.23 to 7.67, and unclear notes per run fell from 52 to 42. The model-facing surface grew
  from 6,992 to 7,440 tokens (the budget is 7,450).
- **Looser contract tests for agent text.** The protocol tests check the guardrails, limits and
  identifiers agents act on (`approvedSizeBytes`, `job_id`, `scope.revision`, file caps), not whole
  sentences, so the wording can improve without breaking them.
- **Benchmark config names the model once.** In `scripts/bench/vendors.example.json`, Ollama and
  FreeLlama inherit `model.ollama_tag`. The bench README says any GGUF chat model works, checked
  with `preflight --hash-model`.
- **One benchmark, with a bottom line.** [Benchmark](docs/BENCHMARK.md) now holds a single run
  (2026-11-14, this build): what to use when, and a KPI table with real numbers for six setups.
  Pooled medians: Ollama 272 ms, FreeLlama streamed 273, FreeLlama 274, llama-swap 332, llama.cpp
  336, LocalAI 802; all 360 answers correct and identical. It adds throughput at 1 and 4 clients,
  cold requests, decode speed, FreeLlama's overhead and its server's memory. The earlier runs and
  their reports were removed. README charts: `docs/assets/benchmark-*.png`, drawn by
  `scripts/bench/readme-charts.mjs`.
- **A measured scorecard.** FreeLlama is rated on 11 measured KPIs (quality, time, first token,
  load, switching, CPU, memory, GPU placement), each against parity with Ollama or a fixed bar:
  9 met, 2 missed (cold request +7%, one unverified placement). The built-in workflow prompts were
  measured against one-line prompts on `bonsai-2:9b`: 45/46 completed passes (98%; 45/48 nominally
  planned) vs 25/48 (52%) for the baseline
  (`scripts/local-test/baseline-prompts.mjs`).
- **Feature comparison.** [Benchmark](docs/BENCHMARK.md#features-beyond-speed) compares ten runners
  on memory checks, model suggestions, MCP, APIs, streaming and metrics, checked against their
  docs on 2026-11-14. It notes that llama.cpp 1.0.0 and Ollama 0.40.16 shipped after the run.

## [0.12.0] - 2026-11-13

### Added

- **Fallback chains.** `run_task {fallbackModels}` (HTTP `fallback_models`, `freellama task
  --fallback-model`, `freellama run --fallback`) lists up to 4 models to try in order. The next one
  runs only after the one before was refused before anything executed (no room by the wait deadline,
  input over its context window, model file over the host budget, not installed, wrong capability,
  circuit open); any other error ends the chain, so no work is replayed. Later models in the chain
  are protected from eviction while an earlier one is tried, each attempt names the model it really
  used (a routed pick included, and never tried twice), and a failed chain's usage is charged to the
  model that failed last. The receipt's `fallback` lists every
  attempt, and the answer (batch items included) gets a `check:` line naming the model that answered.
- **Your own workflows.** Markdown files in `~/.octocode/freellama/workflows/` (or
  `FREELLAMA_WORKFLOWS_DIR`): front matter names the job (`summarize`, `coding`, `ocr` or
  `image-recognition`), title and optional `think`, `max_output`,
  `prefer`, `format`; the body is the instructions. They run through MCP, `freellama run -w` and the
  gateway; one named like a built-in replaces it; invalid files are skipped and named by
  `freellama workflows` and `doctor`; `job_profile.workflow_source` names the file and digest.
- **Idempotency keys from MCP and the agent commands.** `run_task {defer:true, idempotencyKey}` and
  `freellama run --defer --idempotency-key KEY` pass the durable-job key that the HTTP API already took.
- **Stable refusal codes** `model_not_installed`, `model_not_eligible` and `model_exceeds_host_budget`.
- **Sequential benchmark.** `docs/BENCHMARK.md` reviews the competing local runtimes and
  lifecycle layers and records a measured run. Ollama, FreeLlama, llama.cpp, llama-swap and
  LocalAI ran on the same hash-verified GGUF in two opposite orders. All five answered 60/60
  correctly. FreeLlama's managed path was within 0–45 ms of Ollama; LocalAI was about 3× slower.
  Each order has a standalone visual report. The runner now treats macOS `EPERM` from an owned
  process group whose leader has exited as stopped, instead of halting.
- **CI.** `.github/workflows/ci.yml` runs build, type check, Clippy, JavaScript and Rust tests and the
  release package checks on Ubuntu and macOS.

### Changed

- A decider confidence under 0.9 cites the "right about 95% of the time" figure only for
  strands-decider, which publishes it; for any other decider the `check:` line calls its confidence
  uncalibrated.
- The comparison and README credit what Ollama, llama.cpp, llama-swap, LocalAI, Lemonade, LM Studio,
  oMLX and Docker Model Runner already do, and drop claims of uniqueness.

### Fixed

- **Inference replay safety.** Accepted inference and mutation requests no longer retry HTTP 500/502 or follow redirects. Connection-establishment retries and safe read retries remain; uncertain upstream outcomes explicitly report retry unsafe.
- **Job and sweep recovery.** Failed polling retains accepted job identity and unknown execution; sweeps respect structured retry evidence instead of retry-looking error text.
- **Embedding reuse and output bounds.** Indexed retrieval verifies execution identity, preserves explicit routing controls and rejects invalid cached vectors. Large nested response records use bounded previews with lossless record selection.
- **Placement and stream evidence.** Post-run observations match digest/context, and malformed managed stream frames preserve partial output without claiming completion.
- **Benchmark validity.** Added managed completion measurement and adversarial guards for model attribution, unknown execution, process cleanup, bounded capture and prerequisite evidence. No comparative performance win is claimed.
- Docs said deferred jobs are always lost on restart; with `FREELLAMA_DURABLE_JOBS=1` finished
  receipts survive and unfinished ones read back as an unknown outcome. Monitoring said low available
  RAM only warns on macOS; it refuses a new load by default on every OS (`--low-memory advisory`
  restores the warning).

## [0.11.0] - 2026-11-10

### Added

- **Agent commands in `freellama`.** `npm install -g @octocodeai/freellama` now also gives:
  - `freellama run`: one task with `ollama run` flags plus workflows (`-w`), jobs (`-j`), files (`-f`),
    structured output (`--schema`), decisions (`--ask`, `--choice`, `--level`), batches (`--each`),
    background jobs (`--defer`), `--dimensions` and fill-in-the-middle (`--suffix`). It streams on a
    terminal and prints only the answer in a pipe.
  - `freellama chat`: the `ollama` chat on a workflow (`freellama/review-code`) or a tag.
  - `freellama gateway`: workflows served as models (`freellama/<name>`) over the Ollama, OpenAI Chat,
    OpenAI Responses and Anthropic Messages APIs, with native token streaming.
  - `freellama workflows`, `job ID [--wait|--cancel]`, `tool NAME JSON`, `tools`, and a human
    `status` view on a terminal (`--json` or a pipe keeps the JSON receipt).
  - The npm launcher runs these in Node from `@octocodeai/freellama-mcp-server`, now a dependency of
    `@octocodeai/freellama`; every other command stays on the Rust binary.
- **Ollama pre-flight.** When Ollama is not installed, `run`, `chat`, `gateway` and `status` stop at
  once with `Please install Ollama: https://ollama.com/download` (exit 1); when it is installed but
  stopped, they say how to start it (exit 75). The MCP server's unreachable-Ollama error names the
  install link too.
- **Exit codes.** 0 answered, 75 busy or short of memory (retry), 1 refused or failed, 2 usage,
  130 interrupted.
- **Long-poll jobs.** `GET /_freellama/v1/jobs/{id}?wait_ms=N` (up to 5 s) returns as soon as the job
  ends; a short task on a loaded model finishes in about 0.2 s.
- **Persistent embedding index** for `find-code` in `${FREELLAMA_HOME}/embedding-index/`: a repeated
  search embeds only changed chunks (89 chunks: 7.3 s first, 0.28 s next). `FREELLAMA_EMBED_INDEX=0`
  turns it off.
- **Embedding `dimensions`** on `run_task` and the CLI.
- `doctor` reports `OLLAMA_DEBUG_LOG_REQUESTS` and `LLAMA_ARG_CACHE_RAM` posture.

### Changed

- Smaller `run_task` schema (12,180 → 7,958 characters) and receipts (a single result 2,279 → 1,624
  characters; a five-item batch 11,042 → 6,079).
- Streamed and raw requests keep the resident runner: `serve` learns each model's default context
  window, and the gateway matches serve's `shift` setting, so Ollama's prompt cache survives between
  turns (proxy overhead about 55 → 2 ms).
- Workflow prompts: no-padding instructions, `think:false` where reasoning only adds tokens, and the
  capable pick for `change-code` and `summarize-text` (workflow eval: 1.7B model 7/12 → 11/12).
- `check:` lines flag padding and filler.

### Fixed

- The gateway no longer crashes on a client abort, aborts the upstream request, and refuses rebound
  DNS names.
- An image-only generate is no longer treated as a model preload.
- `freellama run` no longer waits on an agent's open, empty stdin.

## [0.10.0] - 2026-11-09

### Added

- **The caller chooses where an answer goes.** `run_task` (single and batch) and `delegate_research` take:
  - `output`: `auto` (default), `inline` or `file`.
    - `auto` returns answers of up to 8,000 characters (about 2,000 tokens) inline.
    - A longer answer comes back as a short view: its opening lines, every `check:`, `verification:`,
      `cited:` and `handoff_file:` line, and the path of the file holding the whole answer.
    - `inline` always returns the whole text. `file` always returns the short view.
    - Hosts limit MCP output (Claude Code warns above 10,000 tokens and caps output at 25,000 by
      default), and long local results cost the calling model tokens.
  - `outputPath`: also write the answer as Markdown to a path the caller chooses, creating or replacing
    it. It is returned as `output_file`.
    - The path must resolve inside the readable roots and outside `.git`.
    - It is checked before any model work runs.
- **Default answer file.** A shortened answer without `outputPath` is written to
  `${FREELLAMA_HOME}/tmp/<timestamp>-<pid>-<n>.md`; `FREELLAMA_HOME` defaults to `~/.octocode/freellama`.
  - That directory is a readable root, so a later `run_task {files:[output_file]}` can chain on it.
  - Files older than `FREELLAMA_RESPONSE_TTL_DAYS` (default 7) are pruned.
- A shortened result carries `output: {mode, chars, file}` in `structuredContent`.

### Changed

- Long `run_task` and `delegate_research` answers are no longer returned whole by default. Pass
  `output: "inline"` for the previous behavior.
- An answer is never lost. If no file can be written, the view falls back to the saved response
  (`read_response`). If the response store is off as well, the whole text stays inline with a note.
- Tool-description token budgets were raised for the two new fields: model-facing surface 7,250 to
  7,450, `tools/list` 8,350 to 8,600.
- A shortened answer no longer repeats in `structuredContent`: its text there becomes
  `answer moved: <location>`. Receipts move telemetry and nested metrics to the saved response file.
  A 10k-character answer now returns under 4,000 characters in total, down from about 24,000.
- `task_jobs {action:"get"}` compacts a finished job and shortens its answer like `run_task`, with a new
  `output` argument.
- `models {view:"installed"}` and `resident` return a slim per-model set (26,384 to 4,150 characters for
  15 models). `includeVerbose:true` returns the full records.
- `delegate_research` without `model` uses the first research-agent catalog pick (stronger, default,
  small) that is installed and fits now, instead of generic coding routing. The receipt reports
  `researchModel`.

### Fixed

- GitHub research answers were checked against the local disk, so correct remote paths were flagged
  "unverified" and the model got a needless repair turn. They are now grounded against the adapter's
  own tool results.
- A sweep whose server process exited now reports `interrupted` with the exact resume call, instead
  of `running`. Sweeps record their owner process, so another live server's sweep is not resumed twice.

## [0.9.0] - 2026-11-09

Reconstructed from the commit history (no release tag exists); see `git log` for detail.

- Job mode and built-in local prompts (`run_task {job}` / `{workflow}`), handoffs (`saveAs`), the
  isolated `coder` research adapter (operator opt-in) and a single owner for local-model prompts.
- Result-derived `check:` lines, and family-based, model-aware local prompts.
- Research agents: structured actions, the GitHub adapter, a small-model prompt and an answer contract;
  grounded research answers and sandbox fixes.
- `sweep`, size-checked `ollama_pull`, model-affinity admission and memory bounds.

[0.10.0]: https://github.com/bgauryy/FreeLlama/compare/0d59c04...HEAD
[0.9.0]: https://github.com/bgauryy/FreeLlama/commit/0d59c04
