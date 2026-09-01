# Coding-agent context management

Load before changing `delegate_research.agent`, any `FREELLAMA_AGENT_*` context setting, or
interpreting `contextManagement`. Why: an unsafe estimate lets Ollama silently truncate the
system contract, while over-compaction destroys useful evidence and prefix-cache reuse.

The first call uses `charsPerToken` (default 4) because Ollama has no stable preflight tokenizer
API. Every successful call calibrates the estimate upward from Ollama's real
`prompt_eval_count`; calibration never lowers the estimate. Calibration is persisted per model
template in `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR`, contains no prompt text, and is reused by later
adapter processes. Read `token_counting`,
`estimate_scale`, and `calibration_samples` in the result rather than assuming exact counting.

Budget: `contextTokens - outputTokens - safetyMarginTokens`. Older observations become
breadcrumbs first, two recent observations stay verbatim when possible, and full tool output
remains retrievable through byte-identical pages. `compactRetainRatio:0.8` means each emergency
pass retains 80% of the current largest observation; it is not an 80% activation threshold.

System prompt and original question are byte-preserved by default. If they cannot fit,
`pinnedOverflow:"error"` fails before Ollama runs. Use `"clip"` only with explicit acceptance that
the operating contract or task may be shortened; increasing `contextTokens` or narrowing the task
is safer.

Tune per call with `delegate_research.agent`; use `FREELLAMA_AGENT_*` for deployment defaults.
Every value is typed and validated. Keep confinement, JSON-only actions, and read-only behavior
fixed: those are safety invariants, not knobs. Re-run the 65 context contracts and held-out
research smoke after changing defaults.

Upstream API status, local boundary results, and reproduction commands:
`assets/evidence/context-management.md`.

Next: KV/memory cost of a larger context → `references/ollama-config.md`; active failures →
`references/troubleshooting.md`.
