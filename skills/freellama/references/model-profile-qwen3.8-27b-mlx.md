# Qwen 27B capability profile

Load before trusting `qwen3.8:27b-mlx` unsupervised or profiling another model. Why: its confidence
does not distinguish strong retrieval from weak judgment.

- Strong: grounded lookup, strict instruction following, false-premise resistance, long-context
  retrieval, and JSON action emission.
- Conditional: use `think:true` and a generous output budget for multi-step reasoning.
- Weak: unsupervised code review produced about one-third false or context-limited findings.
- Tool choice: bash and Octocode tied at 26/30, but bash used fewer calls, tokens, and seconds.
- Scale: corrected reasoning accuracy was 87/88 when infrastructure returned an answer; 12/100
  long-run trials failed at the infrastructure layer.

Do not transfer these results to another tag, quantization, Ollama version, or machine without a
held-out rerun. Full question-level evidence and verifier corrections:
`assets/evidence/model-profile-qwen3.8-27b-mlx.md`.

Next: delegate/verify policy → `references/task-delegation.md`.
