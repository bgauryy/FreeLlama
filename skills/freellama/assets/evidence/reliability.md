# Reliability: retry, backoff, timeout, process restart

Load before touching retry count, backoff, or timeout behaviour. Implemented in
[`proxy.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/proxy.rs) and [`platform/mod.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/platform/mod.rs).

Ollama occasionally returns `HTTP 500` under sustained multi-turn tool-calling load, independent of
memory pressure — measured here at roughly an 8% per-request rate in a 60-trial run with a single
resident model. There is no fix available to us in Ollama itself; the proxy absorbs it.

## What's implemented

- **Retry:** up to `MAX_ATTEMPTS = 3` total attempts on HTTP 500/502/504 or a connection-level
  error. Not retried: 4xx, **HTTP 503** (busy — retrying it amplifies the load the admission
  semaphore is shedding), and success. Same rule on the passthrough and the managed `/tasks` path
  (`proxy::retryable_upstream_status`).
- **Backoff:** exponential with jitter — `RETRY_BASE_DELAY (200ms) * 2^(attempt-1) + jitter(0-100ms)`.
  Jitter is dependency-free (derived from the system clock's low bits, not cryptographic) — its only
  job is to stop multiple retrying callers from piling back onto a recovering server in lockstep.
- **Per-attempt timeout:** `--request-timeout-seconds` on `npx freellama proxy`/`serve` (default 120s).
  This exists specifically because retries without a timeout compound badly: a request that's
  already slow (15-18s observed for a multi-turn chat call near Ollama's failure point) times out
  a whole *task* budget if retried 2-3 times with no per-attempt cap. Verified fix, live:
  `L04` went from `timeout` (180s+, zero salvageable data) to `passed` (50s) once the timeout was
  added on top of retry.
- **Request body buffering:** bodies are buffered (`axum::body::to_bytes`, 64MB cap) rather than
  streamed, because a retried attempt must resend the exact same bytes and a streamed body can only
  be consumed once. Response bodies remain streamed — only the (typically small, JSON) request side
  gives up pure streaming.

- **Shared by both callers.** The passthrough (`proxy::send_with_retries`) and the managed-task
  path (`platform::post_json_with_retries`, behind `/_freellama/v1/tasks`) use one backoff schedule,
  `proxy::retry_delay`. They keep separate `reqwest::Client`s on purpose — a managed generation needs
  a 900s budget, discovery calls need 30s — but the retry policy itself is deliberately not
  duplicated: they hit the same Ollama, and a schedule that drifted on one of them is the kind of
  bug nobody notices until it hurts. The managed path was retry-less until this was unified, which
  was the worse half of the asymmetry, because it holds the `managed_execution` admission permit
  across the upstream call: a bare failure also threw away a slot it had already queued for.
- **Upstream error bodies survive.** A wedged Ollama runner does not always answer in JSON. The
  managed path parses leniently and falls back to carrying the body through as text, so a truthful
  500 stays a 500 instead of collapsing into a misleading 502 "decode error" that points debugging
  at the wrong layer.

## What's deliberately NOT implemented, and why

- **No circuit breaker / retry budget.** These matter most for high-concurrency multi-tenant
  gateways where a thundering herd of retries can DoS a recovering dependency. This proxy currently
  serves one sequential local client (a benchmark, or a single developer's session) — there's no
  concurrent herd to protect against yet. If this proxy ever serves multiple concurrent agents,
  revisit this; a retry-budget cap (e.g. "retries may not exceed 10% of total traffic") is the
  standard next step.
- **No adaptive/dynamic tuning of retry count or backoff based on observed error rate.** Fixed
  constants were chosen deliberately (named as a known gap in [`docs/OLLAMA_SYSTEM_OPTIMIZATION.md`](https://github.com/bgauryy/FreeLlama/blob/main/docs/OLLAMA_SYSTEM_OPTIMIZATION.md) before this work started).

## Third layer: process-level recovery (opt-in)

Retry/backoff above only helps when Ollama is *up but erroring*. `npx freellama proxy
--auto-restart-ollama` extends this one layer down: on a true connection-refused failure
(`reqwest::Error::is_connect()` — the process is gone, not just slow or returning a 5xx), the
proxy quits and relaunches the macOS Ollama app once (quit the app, reopen it), then retries the request once more. A 5-minute cooldown
(`RESTART_COOLDOWN`) caps this at one attempt per outage, not a restart loop. Off by default — this never fires unless explicitly enabled with the
flag. See `## Tests` below for its coverage.

## Two-layer defense: proxy + adapter

Both research adapters ([`bash_agent.py`](https://github.com/bgauryy/FreeLlama/blob/main/benchmark/local/scripts/bash_agent.py), [`octocode_agent.py`](https://github.com/bgauryy/FreeLlama/blob/main/benchmark/local/scripts/octocode_agent.py)) add a *second*, slower retry layer (1
extra retry, 5s backoff) around each chat call, on top of the proxy's own. This exists because a
single sustained outage (~45s, longer than the proxy's 3-attempt budget) was observed to exhaust the
proxy's retries and still fail the whole multi-turn conversation — losing 5-9 turns of accumulated
tool-calling progress to one bad turn is wasteful. Keep these two layers conceptually separate: the
proxy retry is transport-level (any client benefits automatically); the adapter retry is
conversation-level (specific to not discarding in-flight agent state).

## Tests

[`proxy_contract.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/tests/proxy_contract.rs) — TDD, all red-then-green:
`proxy_retries_transient_upstream_errors_and_eventually_succeeds`,
`proxy_gives_up_after_max_attempts_on_persistent_failure`,
`proxy_times_out_a_hung_upstream_instead_of_blocking_forever`,
`proxy_restarts_ollama_once_after_a_connection_refused_failure`,
`proxy_does_not_restart_ollama_when_auto_restart_is_disabled`,
`proxy_does_not_restart_ollama_for_an_ordinary_5xx_not_a_dead_process`,
`proxy_does_not_retry_upstream_503`. Managed-path twin:
`managed_task_does_not_retry_upstream_503` in `platform_contract.rs`. Any future change to
retry/backoff/timeout/restart behavior should add a failing test here first.

Next: seeing a symptom rather than planning a change → `references/troubleshooting.md`. Why the
managed path needed unifying at all → `references/proxy-vs-serve.md`.
