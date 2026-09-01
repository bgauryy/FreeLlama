# Local benchmark: Octocode tool vs. raw shell

**One-line summary:** the same local Ollama model (`qwen3.8:27b-mlx`) answers the same 30 code-research
questions across three pinned repos — [`click`](https://github.com/pallets/click) (Python CLI
framework), [`zustand`](https://github.com/pmndrs/zustand) (TypeScript state management), and
[`openui`](https://github.com/thesysdev/openui) (TypeScript generative-UI monorepo) — twice: once
with the `octocode` CLI as its only research tool, once restricted to raw Linux/bash commands only.
The benchmark records tokens, tool calls, wall time, and deterministic correctness for each
condition. An optional, non-local judge pass is separate from `run_all.sh`.

This self-contained benchmark reuses the scoring and aggregation engine in this repository
(`benchmark/harness/scripts/{run.py,run_matrix.py,aggregate.py,render_html.py}`) and adds:

- two new agent adapters (`scripts/octocode_agent.py`, `scripts/bash_agent.py`), sharing
  `scripts/agent_context.py` for context budgeting, output clipping and JSON-repair — contracts in
  `scripts/test_agent_context.py` (`python3 scripts/test_agent_context.py`). Bash denylist:
  `scripts/test_bash_confine.py`.
- one new 30-question task suite spanning 3 repos (`tasks/octocode-vs-bash-30.json`)
- a matrix **template** pairing both adapters against one model
  (`tasks/octocode-vs-bash-matrix.template.json`; `run_all.sh` fills `__MODEL__` and writes
  `tasks/.generated/`)
- its own pinned clones of the three target repos, cloned by the **runner**, never by the agents
  themselves (`.context/`, gitignored — see "Who clones what" below)

Everything needed to understand or re-run this — for a human or another agent — lives under this
directory. Start here, then read `docs/` in order.

## Directory map

```
benchmark/local/
├── README.md                      <- you are here
├── .gitignore                     <- ignores .context/ and results/
├── docs/
│   ├── 01-flow.md                 <- end-to-end run flow, step by step
│   ├── 02-agent-a-octocode.md     <- Agent A spec: the octocode tool prompt (verbatim)
│   ├── 03-agent-b-bash.md         <- Agent B spec: the bash-only prompt (verbatim)
│   ├── 04-questions.md            <- index of all 30 questions, linking to per-question files
│   ├── 05-grading-and-judge.md    <- deterministic checks + LLM-judge methodology
│   ├── 06-results.md              <- where numbers live (gitignored dashboards + skill notes)
│   └── questions/
│       ├── click/Q1.md .. Q10.md      <- one file per question, prompt only, no answers/checks
│       ├── zustand/Q1.md .. Q10.md
│       └── openui/Q1.md .. Q10.md
├── tasks/
│   ├── octocode-vs-bash-30.json                <- the 30-task suite (prompts + grading)
│   └── octocode-vs-bash-matrix.template.json   <- matrix template; run_all.sh fills in the model
├── scripts/
│   ├── prepare_repo.sh             <- RUNNER-only: clones/pins all 3 repos into .context/
│   ├── restart_ollama.sh           <- restarts the local Ollama server
│   ├── agent_context.py            <- shared: context budget, head+tail clipping, repeat + JSON repair
│   ├── test_agent_context.py       <- contracts for the above (no model needed)
│   ├── test_bash_confine.py        <- denylist contracts for bash_agent.py
│   ├── octocode_agent.py           <- Agent A adapter (Ollama + octocode CLI)
│   ├── bash_agent.py               <- Agent B adapter (Ollama + raw shell only)
│   └── run_all.sh                  <- runs everything for one model: matrix -> aggregate -> render
├── runs/index.jsonl                <- generated local ledger: one line per run_all.sh call
├── .context/                       <- gitignored; created by prepare_repo.sh (click/zustand/openui)
└── results/<model-slug>/           <- gitignored; created by run_all.sh, one subdir per model tested
```

## Who clones what (important)

`.context/` is populated **once, up front, by `scripts/prepare_repo.sh`** — a plain git clone run by
whoever operates this benchmark (a human or the orchestrating harness), before any model runs. It is
gitignored because it's regenerable, large, and not part of the benchmark's source of truth.

Neither Agent A nor Agent B ever runs `git clone`, has network access, or sees `.context/` directly —
each trial gets its own disposable copy of `.context/` (via `run.py`'s fixture-copy mechanism) as its
`FREELLAMA_BENCH_WORKSPACE`. The local model only ever sees a plain directory of already-present
files; it cannot fetch anything itself.

## Why this exists

The repo already has a mature generic benchmark harness (`benchmark/harness/`). This benchmark asks
a narrower question: for the **same model**, does giving it a purpose-built code-research tool
(`octocode`) change how efficiently
and accurately it can answer questions about real codebases, compared to giving it nothing but a
shell? It reuses the same scoring engine so the two studies are numerically comparable.

## Quickstart (re-running this later)

```bash
cd benchmark/local
./scripts/prepare_repo.sh                     # idempotent; clones/pins the 3 repos into .context/
./scripts/restart_ollama.sh                   # Ollama + 11435 (serve if already up, else proxy)
./scripts/run_all.sh --model qwen3.8:27b-mlx  # runs both agents x 30 questions for this model
open results/qwen3.8-27b-mlx/index.html       # view the dashboard
```

**Testing a different model is a one-flag change** — `./scripts/run_all.sh --model qwen2.5:7b` —
nothing to hand-edit. Each model gets its own `results/<model-slug>/`, and every invocation appends
one line to `runs/index.jsonl` recording the run IDs, model, date, pass rate, and data location.
The script creates this local ledger; Git does not track it unless you add it intentionally.

See `docs/01-flow.md` for what each step does, and `docs/05-grading-and-judge.md` for how
the (non-local, post-hoc) LLM judge pass works.

## Adapter loop behaviour these numbers depend on

Both adapters share `scripts/agent_context.py`. Four of its behaviours change results materially,
so a run recorded before they existed is not comparable to one after:

- **Context budgeting.** At `num_ctx=8192` a 10-turn conversation overflows the window, and Ollama
  truncates silently *from the front* — dropping the system prompt, after which the agent stops
  emitting JSON. The fitter pins the contract and question, preserves typed tool-message metadata,
  budgets image inputs without charging for base64 length, and refits before the forced final turn.
  Older observations and thinking traces compact first. The initial estimate is configurable and
  calibrates upward from each real `prompt_eval_count`. Pinned system/task bytes are never clipped
  by default: an impossible fit fails before Ollama runs; `PINNED_OVERFLOW=clip` is explicit opt-in.
  Runs recorded before this was fixed understate the model.
- **Head+tail clipping.** Observations used to be tail-sliced (`observation[-3000:]`), discarding the
  start of every directory listing, grep result and file read. Full tool output is stored on disk
  and **paginated** into the model's context (a repo-wide grep is tens of KB; a 3k clip used to
  look like "not found").
- **JSON repair.** A single unparseable turn used to abort the whole run and discard every tool
  result already gathered. It is now corrected in place, bounded at two repairs.
- **Repeat suppression.** An exact repeat of an earlier call is answered from the prior step
  (`status: "repeat"`) instead of spending a turn and a second copy of the same observation.

The adapter prompts also scope searches away from `node_modules`/`target`/`.venv` and treat
`fixtures/` and `mocks/` as scaffolding. Because those prompts were tuned against this repo and the
`.context/` corpora, adapter changes must be graded on [`benchmark/holdout/`](../holdout/README.md)
instead — fresh upstream repos the prompts were never fitted to.

See `AGENTS.md` for the full description of each.

Every runtime and compaction knob is validated through `FREELLAMA_AGENT_*`; the complete schema,
defaults, per-call MCP mapping, and result metadata are documented in [`AGENTS.md`](../../AGENTS.md).
