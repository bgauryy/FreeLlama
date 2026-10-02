# Retry and recovery

Load before changing retry count, backoff, timeout, or Ollama restart behavior. Why: retries can
amplify overload or turn one slow failure into a task timeout.

- Passthrough and managed tasks use bounded attempts for connection-establishment errors and
  HTTP `500`/`502` with exponential jittered backoff.
- Never retry HTTP 503 while holding admission; it is a load-shedding response.
- Request bodies buffer up to 64 MB for byte-identical replay; responses remain streamed.
- A task deadline covers discovery, queueing, loading, and inference. A per-call deadline is
  capped by the configured server task timeout.
- HTTP `504`, timeouts, and post-send disconnects are final because the upstream generation
  can still be running.
- `--auto-restart-ollama` is opt-in, macOS-only, connection-refused-only, and cooldown-bounded.
- Research adapters add one conversation-level retry so one bad turn does not discard earlier tool
  evidence. Keep this separate from transport retries.

Every behavior change needs a red proxy or platform contract first. Exact constants, incident data,
process-recovery flow, and test names: `assets/evidence/reliability.md`.

Next: symptom diagnosis → `references/troubleshooting.md`; mode boundary →
`references/proxy-vs-serve.md`.
