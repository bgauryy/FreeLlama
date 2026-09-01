# Troubleshoot FreeLlama

Load when a request fails, slows down, or reaches the wrong route. Why: similar symptoms come from
different layers.

1. Run `doctor`, then `scripts/check.sh`.
2. HTTP 503: inspect the selected backend's admission pool; retry or lower fan-out. GPU defaults to
   2 weighted units and CPU to 1. Raise only the relevant pool after reading Ollama parallel/KV
   constraints.
3. Low-confidence refusal: add policy plus functional evidence, lower the requested floor, or use
   fastest with an explicit acknowledgment of low evidence.
4. Slow but successful: inspect `execution.observation`, not assignment alone. Partial offload is
   silent; free memory or lower context. A CPU-assigned model observed in VRAM is a mismatch and its
   timing is withheld from adaptive feedback.
5. Intermittent HTTP 500: check co-resident memory first, then use FreeLlama retry protection.
6. 404 on control routes: use `serve`, not passthrough-only `proxy`.
7. Research root refusal: keep `workspacePath` inside an allowlisted root; never widen to `$HOME`
   or `/` for convenience.
8. CLI/server version mismatch: restart or align installations before trusting CLI-specific flags.
9. Shared-output corruption: stop straggler runs before starting another writer.

Observed incidents, exact remedies, and error-specific timing evidence:
`assets/evidence/troubleshooting.md`.

Next: retry mechanics → `references/reliability.md`; backend decisions →
`references/resource-routing.md`.
