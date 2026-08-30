# Local Benchmark: Octocode Tool vs. Raw Shell

**One-line summary:** the same local Ollama model (`qwen3.8:27b-mlx`) answers the same 30 code-research
questions across three pinned repos — [`click`](https://github.com/pallets/click) (Python CLI
framework), [`zustand`](https://github.com/pmndrs/zustand) (TypeScript state management), and
[`openui`](https://github.com/thesysdev/openui) (TypeScript generative-UI monorepo) — twice: once
with the `octocode` CLI as its only research tool, once restricted to raw Linux/bash commands only.
We measure tokens, tool calls, wall time, deterministic correctness, and an LLM-judge quality score
for each condition.

This is a new, self-contained benchmark. It reuses the scoring/aggregation engine already in this
repo (`benchmark/harness/scripts/{run.py,run_matrix.py,aggregate.py,render_html.py}`) but adds:

- two new agent adapters (`scripts/octocode_agent.py`, `scripts/bash_agent.py`)
- one new 30-question task suite spanning 3 repos (`tasks/octocode-vs-bash-30.json`)
- one new matrix pairing both adapters against the same model (`tasks/octocode-vs-bash-matrix.json`)
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
│   ├── 06-results.md              <- filled in after the run; the actual findings
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
│   ├── octocode_agent.py           <- Agent A adapter (Ollama + octocode CLI)
│   ├── bash_agent.py               <- Agent B adapter (Ollama + raw shell only)
│   └── run_all.sh                  <- runs everything for one model: matrix -> aggregate -> render
├── runs/index.jsonl                <- tracked ledger: one line per run_all.sh call (runId+model+date)
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

The repo already has a mature benchmark harness (`benchmark/harness/`) that was used to compare
several Ollama models on a fixed `list/search/read/edit` JSON tool contract
(`.octocode/benchmarks/real-repos-2026-08-24/`). This benchmark asks a different question: for the
**same model**, does giving it a purpose-built code-research tool (`octocode`) change how efficiently
and accurately it can answer questions about real codebases, compared to giving it nothing but a
shell? It reuses the same scoring engine so the two studies are numerically comparable.

## Quickstart (re-running this later)

```bash
cd /Users/guybary/Documents/code/FreeLlama/benchmark/local
./scripts/prepare_repo.sh                     # idempotent; clones/pins the 3 repos into .context/
./scripts/restart_ollama.sh                   # restarts Ollama so the run starts from a clean state
./scripts/run_all.sh --model qwen3.8:27b-mlx  # runs both agents x 30 questions for this model
open results/qwen3.8-27b-mlx/index.html       # view the dashboard
```

**Testing a different model is a one-flag change** — `./scripts/run_all.sh --model qwen2.5:7b` —
nothing to hand-edit. Each model gets its own `results/<model-slug>/`, and every invocation appends
one line to `runs/index.jsonl` recording the run_id(s), model, date, pass rate, and where the data
lives, so past runs stay discoverable even after you've moved on to testing a different model.

See `docs/01-flow.md` for what each step actually does, and `docs/05-grading-and-judge.md` for how
the (non-local, post-hoc) LLM judge pass works.
