# Delegation boundary

Load before deciding what to hand a local model or trusting its answer. Why: retrieval, judgment,
and embeddings have sharply different measured reliability.

Delegate grounded lookups, long-context retrieval, bulk transforms, OCR, and embeddings. Keep
judgment, design, ambiguous synthesis, and hidden multi-step reasoning with the orchestrator—or
verify them independently.

Use `run_task` for content already in hand. Use `delegate_research` only when the model must read
allowlisted files and return citations. Read `verification`, successful call counts, and full
citations; failed calls do not ground a claim.

`grep` wins when code has a guessable identifier. Embeddings win when no keyword exists: grouping,
similarity, and dedup candidate generation. A high cosine score is a candidate, never a deletion or
equivalence verdict.

Measured ranking: embeddings are strongest and cheapest; grounded questions pay off beyond about
1,000 source tokens; code review measured about 67% and always needs verification. The local model
does runtime delegation, not training-time knowledge distillation.

Full measurements, negative RAG results, result-field contract, and external cost sources:
`assets/evidence/task-delegation.md`.

Next: model-specific evidence → `references/model-profile-qwen3.8-27b-mlx.md`; model choice →
`references/model-selection.md`.
