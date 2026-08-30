# Agents

FreeLlama includes specialized agent adapters for benchmarking and research, living entirely
under [`benchmark/local/`](benchmark/local/README.md) — the one canonical home for them, built on
the generic scoring infrastructure in [`benchmark/harness/`](benchmark/harness/README.md) (see
[`benchmark/harness/README.md`](benchmark/harness/README.md) for the workflow map).

## Available Agents

### Octocode CLI Agent (v1)

**ID:** `octocode-cli-agent-v1`

**What it is:** An Ollama model with access to a purpose-built code-research tool (`octocode` CLI,
invoked via `npx octocode`) that provides five structured operations: local file viewing, finding
files by pattern, code search, semantic content retrieval, and LSP-based queries.

**Location:** `benchmark/local/scripts/octocode_agent.py`

**Tool set:**
- `localViewStructure` — View repository directory tree or file structure
- `localFindFiles` — Find files matching patterns (by name, extension, path)
- `localSearchCode` — Full-text code search with context
- `localGetFileContent` — Read file with syntax and line numbers
- `lspGetSemantics` — Language server protocol queries (definitions, references, hover) — rarely
  used in practice; see `skills/freellama/references/model-profile-qwen3.8-27b-mlx.md` for why.

**Decoding/runtime config (hardcoded constants in the script, not env-var-configurable today):**
`temperature=0`, `seed=42`, `num_ctx=8192`, `num_predict=512`, `max_turns=10`,
`tool_timeout_seconds=45`, `tool_result_truncate_chars=2000`, `observation_truncate_chars=3000`.
To change these, edit the constants directly in `octocode_agent.py` — there is no config file or
env-var layer for them (a prior parameterization effort planned one but never wired the adapters
to read it; don't trust docs elsewhere that say otherwise).

**Use when:** You need an agent to answer code-research questions efficiently, with access to
structured code navigation and semantic understanding. The tool provides exact paths and evidence,
reducing hallucination and supporting deterministic grading.

**Performance notes:**
- Tool timeout: 45 seconds per call (longer than bash due to LSP overhead)
- Typically uses fewer tool calls than bash agents for the same questions on an accurate model —
  but not always faster or cheaper: measured evidence (30-question suite, qwen3.8:27b-mlx) showed
  *equal* pass rate to the bash agent while using ~2.8x more time and ~4.7x more input tokens. See
  `skills/freellama/references/task-delegation.md`.
- Results include exact file paths and line numbers

### Bash Shell Agent (v1)

**ID:** `bash-shell-agent-v1`

**What it is:** An Ollama model restricted to raw POSIX shell commands only (ls, find, grep, cat,
awk, etc). No specialized tools; must solve problems using only what the shell provides.

**Location:** `benchmark/local/scripts/bash_agent.py`

**Command restrictions:**
- ✅ Allowed: ls, find, grep, cat, head, tail, awk, sed, sort, uniq, wc, file, locate, etc.
- ❌ Blocked (regex denylist in the script): `sudo`, `rm -rf /`, `curl`, `wget`, `nc`, `ssh`, fork
  bombs, device-file redirects.

**Decoding/runtime config (hardcoded constants, same caveat as above):** `temperature=0`,
`seed=42`, `num_ctx=8192`, `num_predict=512`, `max_turns=10`, `command_timeout_seconds=30`,
`tool_result_truncate_chars=2000`, `observation_truncate_chars=3000`.

**Use when:** You want to measure how well a model can solve code-research problems using only
primitive shell tools, without specialized language-aware features. Useful as a baseline or to
evaluate whether better performance comes from the tool or the model.

**Performance notes:**
- Command timeout: 30 seconds per command
- On the one 30-question suite measured, needed *fewer* tool calls and tokens than the octocode
  agent for equal accuracy — don't assume more structure always wins; verify per task/model.

## Running Agents

Agents are invoked through `benchmark/local/scripts/run_all.sh`, which runs both agents on the
same suite for a given model:

```bash
cd benchmark/local
./scripts/prepare_repo.sh                     # one-time: clone click/zustand/openui into .context/
./scripts/restart_ollama.sh                   # start Ollama + the retry-protected FreeLlama proxy
./scripts/run_all.sh --model qwen3.8:27b-mlx  # runs both agents on all 30 questions
open results/qwen3.8-27b-mlx/index.html
```

See `benchmark/local/README.md` for full documentation.

## Agent Lifecycle

When the benchmark harness (`benchmark/harness/scripts/run.py`) runs an agent:

1. **Setup** — Agent process starts with environment variables set:
   - `FREELLAMA_TARGET_MODEL` — Ollama model tag to use
   - `FREELLAMA_OLLAMA_ENDPOINT` — HTTP endpoint of Ollama (or the FreeLlama proxy)
   - `FREELLAMA_BENCH_WORKSPACE` / `FREELLAMA_BENCH_PROMPT` / `FREELLAMA_AGENT_RESULT` — paths for
     the disposable workspace copy, the question text, and where to write the result JSON
   - Everything else (temperature, seed, timeouts, truncation limits) is a hardcoded constant in
     the adapter script itself, not read from the environment

2. **Task loop** — For each question/task:
   - Agent receives the task prompt and a disposable copy of the fixture repos
   - Agent makes tool calls (octocode) or shell commands (bash)
   - Results are captured (truncated if exceeding limits)
   - Loop continues until max turns reached or the model calls `finish`

3. **Grading** — After the agent completes:
   - **Deterministic checks** run (same for all agents): response contains required keywords,
     claimed file paths exist in the fixture, no workspace mutations
   - **LLM judge**: not run automatically as part of this benchmark. This repo's own incident
     (see `skills/freellama/references/disk-cleanup.md` and `task-delegation.md`) is why: a local
     judge model co-resident with the agent model under test crashed Ollama and corrupted a run.
     Quality judging here is a separate, deliberate, non-local, post-hoc step — see
     `benchmark/local/docs/05-grading-and-judge.md`.

4. **Cleanup** — Agent process stops; the disposable workspace copy is discarded (or, with
   `run_all.sh`'s default `--discard-workspaces`, never even fully persisted)

## Extending with a new agent

1. **Create an adapter** — a Python script that:
   - Reads `FREELLAMA_TARGET_MODEL`, `FREELLAMA_OLLAMA_ENDPOINT`, `FREELLAMA_BENCH_WORKSPACE`,
     `FREELLAMA_BENCH_PROMPT`, `FREELLAMA_AGENT_RESULT` (see `octocode_agent.py`/`bash_agent.py`
     for the exact contract — copy one of them as a starting point)
   - Implements the tool-call loop (receives task, makes calls, gets results, loops)
   - Writes the result JSON to the path in `FREELLAMA_AGENT_RESULT`
2. **Add it to a matrix** — `benchmark/local/tasks/octocode-vs-bash-matrix.template.json` is
   filled in at runtime by `run_all.sh`; add a new model entry there (or a new template) pointing
   `agent_command` at your script.
3. **Test** — run it and verify results appear in the dashboard.

## References

- **Benchmark skill (workflow map)**: [`benchmark/harness/README.md`](benchmark/harness/README.md)
- **Generic harness**: [`benchmark/harness/README.md`](benchmark/harness/README.md)
- **This benchmark (the actual example)**: [`benchmark/local/README.md`](benchmark/local/README.md)
- **Ollama/model operations skill**: [`skills/freellama/SKILL.md`](skills/freellama/SKILL.md)
- **Agent implementation**:
  - Octocode adapter: `benchmark/local/scripts/octocode_agent.py`
  - Bash adapter: `benchmark/local/scripts/bash_agent.py`
