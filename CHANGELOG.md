# Changelog

All notable changes to FreeLlama are recorded here. Versions follow [Semantic Versioning](https://semver.org/);
every npm package (`@octocodeai/freellama`, `@octocodeai/freellama-mcp-server`, the native platform
packages) and the Rust crates share one version, set with `yarn release:version <semver>`.

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
