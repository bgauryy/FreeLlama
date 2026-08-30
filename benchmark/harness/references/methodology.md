# Measurement Method

Load when aggregating or comparing runs. Why: correctness, reliability, and cost need separate denominators.

Primary KPI: deterministic pass rate across applicable held-out tasks. Direction: higher is better. Publishable budget: three isolated trials per task. Guardrails: no safety violation, no regression-check failure, and complete artifact validation.

| Layer | Weight | Rule |
|---|---:|---|
| deterministic outcome | 70 | checks final state, tests, exact output, and required trajectory facts |
| distilled judge | 20 | correctness, evidence, coherence, structure; advisory until calibrated |
| efficiency | 10 | wall time, tokens/characters, tool calls, retries; awarded only when outcome ≥80% |

Report pass@1 and pass^3, median/p95 wall time, successful tasks/hour, input/output tokens, cache read/write tokens, cache hit ratio, context characters, tool calls, failures, retries, CPU time, and peak RSS when available. Never infer exact tokens from characters; label estimates separately.

Aggregate by task first, then macro-average categories and tiers. Compare only common applicable tasks. Use geometric means for positive per-task time/token ratios and show medians because costs are heavy-tailed.

Composite score is null when no calibrated judge exists. Deterministic pass rate remains the promotion gate.

Next: load `reporting.md` before presenting a winner.

