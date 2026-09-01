# Deterministic grading and the LLM judge

The benchmark supports two separate measurement passes. `run_all.sh` performs only the first:

1. **While the local models run:** deterministic checks + resource metrics only. No LLM judging
   happens on-device. This was a deliberate fix — see "Why no local judge" below.
2. **Optional post-hoc review:** after both local agents finish all 30 questions, an independent,
   non-local model can judge each answer against the verified reference. This manual orchestrator
   step is not implemented by `run_all.sh` and is not part of the deterministic score.

## Who runs what (important)

- **Orchestrator:** a non-local model (Claude Code / Codex) — never delegated to a local model.
  It clones the repos, restarts Ollama, launches each local agent run, watches it complete, and
  performs the judging pass afterward.
- **Subjects under test:** local models only (`qwen3.8:27b-mlx` via the octocode agent, then via
  the bash agent), run **one at a time, question by question, never in parallel** — `run.py`
  iterates tasks in a plain sequential loop and `run_matrix.py` runs matrix entries one after
  another through a blocking `subprocess.run`; there is no concurrency to deactivate. The
  [no-local-judge rationale](#why-no-local-judge) explains why the benchmark runs them back-to-back
  without a resident second model.
- **Results are durable per question, not batched at the end:** `run.py` writes one `trial-N.json`
  the moment each question's grading finishes, and prints a `{"task":...,"status":...,"score":...}`
  line to stdout as it happens, so progress and per-question stats are visible and on disk
  throughout the run rather than only after everything completes.

## Why no local judge

The first attempt at this benchmark set `judge_model: "qwen2.5:32b"` in the matrix, so
`distilled_judge.py` scored every trial locally after it ran. On this machine (48GB
unified memory) that meant Ollama had to keep the ~30GB agent model (`qwen3.8:27b-mlx`) resident
*and* load a second ~28GB judge model for every single trial — ~58GB against a 48GB budget. The
result was intermittent `HTTP 500` crashes from Ollama's server (visible in
`~/.ollama/logs/server.log`), which corrupted roughly a third of that run's trials — those trials
failed because the *infrastructure* fell over, not because the agent's tool choice was worse. That's
a real bug, not a measurement. Local-on-local judging on hardware this size isn't safe for this
benchmark, so it's removed from the matrix entirely (`"judge_model": null`) and moved to a separate,
later phase using a model that was never resident on the same machine as the models being judged.

## Why agents talk to a proxy, not raw Ollama

Removing the local judge fixed most of the flakiness, but not all of it: a follow-up run (judge
removed, single model resident, plenty of memory headroom) still hit `HTTP 500` on ~8% of trials
(5/60). At that point, FreeLlama did not retry transient upstream failures. Since this benchmark's
whole point includes measuring the two agents fairly, an infrastructure crash
silently corrupting one condition's trials more than the other's biased the comparison: 4 of the 5
infrastructure errors landed on the octocode agent's longer, more
context-heavy conversations).

The fix went into the actual product, not a benchmark-local workaround: `packages/rust-core/src/proxy.rs` (FreeLlama's
existing Ollama-compatible sidecar) gained retry-with-backoff on transient upstream failures
(HTTP 500/502/504 or connection errors — up to 3 attempts; **not** 503 busy), added test-first
(`packages/rust-core/tests/proxy_contract.rs::proxy_retries_transient_upstream_errors_and_eventually_succeeds` /
`...gives_up_after_max_attempts_on_persistent_failure`, both red before the fix, green after, no
regressions in the full Rust suite). `scripts/restart_ollama.sh` also builds and starts
this proxy (`127.0.0.1:11435 -> 127.0.0.1:11434`), and both agent adapters point
`FREELLAMA_OLLAMA_ENDPOINT` at the proxy instead of Ollama directly — so retries are transparent to
the adapters and identical for both agents.

## Deterministic checks (still the authoritative anchor)

Unchanged from before — every task uses the same three tool-name-agnostic checks
(`response_contains`, `evidence_paths_exist`, `no_changes`; see `04-questions.md`). These are the
"anchor" in eval-methodology terms: a check that ran and produced an unambiguous pass/fail, not a
judgment call. Nothing about the judge redesign touches this.

## The independent judge pass (post-hoc, non-local)

Once both agents have produced all 30 answers, the orchestrator judges every answer independently,
following current LLM-as-judge best practice rather than the harness's original pairwise/local
design:

- **Reference-grounded, not pairwise.** Each answer is scored against this benchmark's own verified
  answer key (author-checked against the pinned source before any model ran — see `04-questions.md`)
  rather than compared head-to-head. Reference grounding is the most reliable mode of LLM judging
  because the judge isn't guessing at correctness, it's checking a claim against a known-true fact.
- **Blind to agent identity.** Each answer is presented to the judge as "Answer 1" / "Answer 2" with
  no mention of which agent, tool, or model produced it — mirroring the sanitized-packet approach
  `distilled_judge.py` already used, extended here to also blind the judge to *order* by scoring each
  answer independently rather than asking "which is better."
- **Independent scoring, not forced comparison.** The judge scores each answer on its own against
  the rubric (0-5 correctness / evidence / completeness, per question, scaled to 0-100), then the
  orchestrator derives the octocode-vs-bash comparison from the two independent scores — this avoids
  the position bias (favoring whichever answer is shown first) that pairwise "A or B" prompts are
  documented to suffer from.
- **Different-family judge.** The judge is Claude/Codex, never Qwen — avoiding the documented
  self-preference/family bias where a judge rates same-family outputs more favorably.
- **Fresh context per question.** Each question is judged by a subagent with no memory of writing
  the question, its answer key, or any other question's grading — matching the eval methodology's
  "verifier independence" requirement (a verifier sharing the executor's context isn't independent).

This mirrors two things at once: general LLM-as-judge practice (reference grounding, blind/
order-randomized scoring, cross-family judges, periodic human calibration) and this repo's own eval
methodology (anchor first, verifier independence, grade outcomes not paths, no Goodhart-able
self-grading).

## What "efficiency" still measures

Unchanged: `aggregate.py`'s 0-10 per-task efficiency score, relative to whichever agent used fewer
tokens/tool-calls/wall-time to reach a *deterministically passing* answer on that question. This
stays a purely mechanical, local-only metric — it was never part of the judge redesign because it
was never broken.

## Sources

- [LLM-as-Judge Best Practices in 2026: Calibration, Bias, and Cost](https://futureagi.com/blog/llm-as-judge-best-practices-2026)
- [LLM-as-a-Judge in 2026: How It Works, When It Fails](https://futureagi.com/blog/llm-as-a-judge/)
- [LLM-Judge Bias Mitigation (2026): Detect, Measure, Fix](https://futureagi.com/blog/evaluating-llm-judge-bias-mitigation-2026/)
- [Mitigating the Bias of Large Language Model Evaluation](https://arxiv.org/pdf/2409.16788)
- [Judging the Judges: A Systematic Evaluation of Bias Mitigation Strategies in LLM-as-a-Judge Pipelines](https://arxiv.org/pdf/2604.23178)
- This repo's `octocode-graph-eval` skill (anchor requirement, verifier independence, Goodhart guard,
  grade-outcomes-over-paths) — applied above to the judge-pass design.
