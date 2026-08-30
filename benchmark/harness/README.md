# FreeLlama coding-agent benchmark


Flow: `PREFLIGHT → RUN → GRADE → AGGREGATE → REPORT → VERIFY`

## Hard rules

- Keep `benchmark/harness/tasks/suite.json`, fixtures, schemas, and graders frozen during a comparison.
- Use a fresh copied workspace per task and trial. Never point a mutating agent at the source fixture.
- Deterministic outcome gates quality; speed cannot rescue an incorrect result.
- Record unsupported requirements as `not_applicable`, and compare only common applicable tasks.
- Run three trials for publishable reliability. One trial is a smoke result.
- Use a judge model different from the candidate family when possible; uncalibrated judge scores are advisory.
- Preserve raw trial JSON. Aggregates and HTML are rebuildable views, not sources of truth.
- Refuse publishable runs after `review_due_at`; review the suite and advance its dates, or use an explicit smoke-only stale override.

These rules were previously duplicated in a `skills/run-benchmark/SKILL.md` workflow map. That file said of itself that it was "the workflow map, not a copy of the mechanics" — so it has been folded in here, next to the mechanics it describes, rather than kept as a second place to look.

This benchmark compares local models running through coding agents. It answers a practical question: which model-agent combination can complete repository work correctly, repeatedly, and efficiently?

It is not a chat benchmark or a raw tokens-per-second test. A model must run through an agent that can inspect an isolated repository, call tools, edit files, and return a final answer. The benchmark scores the complete system: model, agent loop, prompt, tools, skills, and MCP configuration.

## What it measures

The general frozen suite contains 20 synthetic tasks, from basic to complex:

- Basic: structured output, clear explanations, tool selection, small fixes, and repository orientation.
- Core: cross-file search, impact analysis, feature work, restraint, and diagnosis.
- Advanced: multi-file repair, regression handling, recovery from tool failure, efficient investigation, and MCP calls.
- Complex: skill use, instruction conflicts, refactoring, performance work, and end-to-end repository changes.

Deterministic checks grade repository state, tests, exact output, safety constraints, and required tool evidence. A calibrated distilled judge can additionally score correctness, evidence, coherence, and structure. Deterministic pass rate remains the promotion gate.

For successful tasks, the report also records wall time, token usage when supplied by the adapter, cache reads, cache writes, context size, tool calls, retries, and peak memory. Comparisons use only tasks supported by both candidates.

See [`references/tasks.md`](references/tasks.md) for suite coverage and [`references/methodology.md`](references/methodology.md) for scoring rules.

The additional `tasks/real-repos-10.json` suite contains ten coding challenges against pinned Click and ItsDangerous revisions. It measures task understanding, targeted tools, repository tracing, bug diagnosis, five injected repairs, cross-repository research, and one complex two-defect repair. Its local Ollama adapter is `scripts/ollama_repo_agent.py`.

## Verification status

- Harness acceptance test: **passed on 2026-08-24**.
- Golden-reference smoke run: **20 of 20 tasks passed on 2026-08-24**; all 42 generated JSON artifacts validated.
- Public suite content review: **2026-08-23**.
- Next required suite review: **2026-09-22**. The runner rejects stale publishable runs.
- Real repository smoke campaign: **12 installed artifacts checked on 2026-08-24**. The golden reference passed 10/10, every injected defect failed unrepaired, and all 234 final JSON artifacts validated. See `.octocode/benchmarks/real-repos-2026-08-24/index.html` and `.octocode/evals/2026-08-24-real-repository-agent-benchmark.md`.

The golden reference proves that the harness, tasks, graders, aggregation, and report work together. Never report its score as candidate-model performance.

## Check the benchmark

Run commands from `benchmark/harness`.

Run the full acceptance test:

```bash
python3 scripts/self_test.py
```

The test checks schemas, freshness dates, all 20 public tasks across three trials, all 20 generated private variants, judge calibration, aggregation, HTML output, regression detection, and process-tree cleanup after a timeout.

The repository-level verification run writes its checked reference dashboard to `.octocode/benchmarks/reference-check-2026-08-24/index.html`. It is a harness report, not a model comparison.

## Run a model

First connect a non-interactive coding-agent command. The runner expands `{model}`, `{prompt_file}`, `{workspace}`, and `{result_file}`. See [`references/adapters.md`](references/adapters.md) for the normalized result format.

Run one agent/model:

```bash
python3 scripts/run.py \
  --suite tasks/suite.json \
  --model qwen-local \
  --agent-command 'my-agent --model {model} --prompt {prompt_file} --workspace {workspace}' \
  --capability filesystem --capability shell --capability tools \
  --trials 3 \
  --results .octocode/benchmarks/local-agents
```

Then aggregate and render:

```bash
python3 scripts/aggregate.py --results .octocode/benchmarks/local-agents --output .octocode/benchmarks/aggregate.json
python3 scripts/render_html.py --aggregate .octocode/benchmarks/aggregate.json --output .octocode/benchmarks/index.html
```

For several local models, copy `tasks/matrix.example.json`, fill each agent command and capability set, then run:

```bash
python3 scripts/run_matrix.py --matrix tasks/matrix.json --suite tasks/suite.json --trials 3 --results .octocode/benchmarks/local-agents
```

`run_matrix.py` runs models sequentially. It writes `aggregate.json` and a self-contained `index.html` in the results directory. The adapter can write normalized metadata to `FREELLAMA_AGENT_RESULT`; otherwise the runner treats standard output as the final answer and cannot report exact token or tool metrics.

Build a focused dashboard without changing the authoritative aggregate:

```bash
python3 scripts/render_html.py \
  --aggregate <results>/aggregate.json \
  --suite tasks/real-repos-10.json \
  --output <results>/selected-models.html \
  --model qwen3.8:27b-mlx --model muse-glimmer:30b-mlx
```

Run the pinned real-repository smoke matrix with the bundled Ollama agent:

```bash
python3 scripts/run_matrix.py \
  --matrix tasks/real-repos-matrix.json \
  --suite tasks/real-repos-10.json \
  --trials 1 \
  --discard-workspaces --skip-complete \
  --results ../../../.octocode/benchmarks/real-repos-2026-08-24
```

`--skip-complete` makes an interrupted sequential campaign safely restartable. `--discard-workspaces` preserves prompts, normalized agent results, diffs, trial JSON, and logs while deleting copied repository workspaces.

Use one trial only for a smoke check. Use three isolated trials per applicable task for a publishable comparison.

## Promotion flow

1. Validate dates, schemas, fixture, and matrix:

   ```bash
   python3 scripts/validate.py --suite tasks/suite.json --matrix tasks/matrix.json
   ```

2. Generate a fresh private suite outside this folder; omit `--seed`:

   ```bash
   python3 scripts/build_private_suite.py --suite tasks/suite.json --output /absolute/path/to/private-benchmark/suite.json
   ```

3. If judge points affect a decision, produce at least 20 human-labeled calibration cases and gate them:

   ```bash
   python3 scripts/calibrate_judge.py --input /absolute/path/to/private-benchmark/judge-labels.json --judge-model <judge> --output /absolute/path/to/private-benchmark/calibration.json
   ```

4. Put the judge model and calibration path in the matrix, then run three trials against the private suite:

   ```bash
   python3 scripts/run_matrix.py --matrix tasks/matrix.json --suite /absolute/path/to/private-benchmark/suite.json --trials 3 --results /absolute/path/to/benchmark-results
   ```

5. Validate raw trials and the generated aggregate and dashboard:

   ```bash
   python3 scripts/validate.py --suite /absolute/path/to/private-benchmark/suite.json --calibration /absolute/path/to/private-benchmark/calibration.json --results /absolute/path/to/benchmark-results
   ```

Keep private suites, labels, prompts, raw secrets, and seeds out of commits, prompts, RAG, and training data. Use the public suite only for development and smoke comparisons.

## Read the results

Open `<results>/index.html` in a browser. Start with deterministic pass rate and `pass^3`, then check capability coverage and safety failures. Compare speed, tokens, cache behavior, and memory only after correctness passes the quality gate.

A model is not automatically the winner because it is fastest. The promotion contract requires at least 80% deterministic pass@1, at least 70% `pass^3`, no guardrail failures, and throughput that is not materially worse than the incumbent. See [`tasks/kpi-contract.json`](tasks/kpi-contract.json).
