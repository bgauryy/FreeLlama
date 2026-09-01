# Model selection and confidence

Load when choosing or installing a model, or when `minConfidence:"medium"` refuses every route.
Why: capability metadata and parameter counts are not quality evidence.

Start with Flow D in `SKILL.md`; it owns request-intent questions, host inspection, candidate
presentation, and exact-tag approval gate. This reference owns how to qualify the candidates:

1. For quality-sensitive work, preview with `minConfidence:"medium"`.
2. Make `medium` reachable with both a task policy from correctness evaluations and a local
   `bench-all` functional report. Neither input substitutes for the other.
3. Generate policy with `policy-from-eval`; never manufacture it from throughput.
4. For library search, fetch families first, then inspect one family for pullable tags and
   `fitsInMemory`. With `serve` down, do not guess a tag.

Fastest routing without evidence is only a capability filter and can select a 0.5B model for code
repair. Vision tags also require a real image trial; names and family claims have failed in both
directions on this setup.

Measured model table, vision trials, embedding comparison, and policy provenance rules:
`assets/evidence/model-selection.md`.

Next: memory fit → `references/ollama-config.md`; delegation scope →
`references/task-delegation.md`.
