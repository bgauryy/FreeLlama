# `freellama proxy` vs `freellama serve`

Load when choosing which mode to run, or when a `/_freellama/v1/*` call 404s and you need to know
whether that route exists at all.

Both bind `127.0.0.1:11435` by default — pick one, not both; the second fails to bind.

| | `npx @octocodeai/freellama proxy` | `npx @octocodeai/freellama serve` |
|---|---|---|
| Ollama passthrough (`/api/*`) | Yes, with retry/backoff/timeout | Yes, same code path — it composes `proxy::app()` as its fallback |
| `/_freellama/v1/{health,machine,models,recommendations,routes,natural-routes,sessions,tasks}` | No — 404 | Yes |
| Retry on the managed task path (`/tasks`) | n/a — route does not exist | Yes, same backoff schedule as passthrough |
| Explicit CPU-model backend | No | Yes, `--cpu-upstream` plus repeatable `--cpu-model` |
| Use when | You only need a more reliable Ollama endpoint (a benchmark, a script calling `/api/chat` directly) | You want model discovery, task-aware routing, admission control, or session affinity |

Every MCP tool except `doctor`, `ollama_manage`, `ollama_delete`, and no-serve model views needs
`serve`. `delegate_research` now sends every adapter model turn through managed `/tasks`, so coding
agents receive the same routing, admission, placement observation, and feedback protection as
`run_task`. `models{view:"resident"}` uses the managed two-backend catalog;
`detail` and `raw` talk to the explicitly selected Ollama endpoint. `run_task` (including
`preview:true`) needs `serve`.

## Which one am I pointed at?

- With an agent: call `doctor`. Chip/RAM come from local sysctl even when `serve` is down.
  `curl -sf $ENDPOINT/_freellama/v1/health` is how you tell `serve` from `proxy` (200 vs 404).
  require `contracts.placement_observation:"ollama_api_ps_after_execution"` and
  `contracts.placement_evidence_gate:"configured_or_observed"` too; missing fields mean stale serve.
- `scripts/check.sh` does this automatically and labels which mode is running.

## Start either one

```bash
npx @octocodeai/freellama proxy                                   # passthrough + retry only
npx @octocodeai/freellama serve --policy-file platform.toml \
                    --benchmark-report bench-all.json # add these to make minConfidence "medium" reachable
```

For concurrent GPU and CPU execution, run a second CPU-only Ollama on another loopback port and
start `serve` with `--cpu-upstream http://127.0.0.1:11436 --cpu-model <installed-tag>`. Managed
catalog discovery, residency, and tasks for that exact tag use the CPU backend. Raw `/api/*`
passthrough still uses the primary `--upstream`; `proxy` has no model-aware routing layer.

In a checkout the npm launcher runs `target/release/freellama` if it is there, so
`cargo build --release` then `npx @octocodeai/freellama …` works unpacked.

## Retry coverage — both paths, one schedule

Retry/backoff/timeout live in
[`proxy.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/proxy.rs)
(`send_with_retries`), and `serve`'s passthrough reuses it, so raw `/api/chat` calls through `serve`
are protected. The managed task path behind `/_freellama/v1/tasks` uses
`platform::post_json_with_retries` but shares `proxy::retry_delay`, `proxy::MAX_ATTEMPTS`, and
`proxy::retryable_upstream_status` — one schedule, including **do not retry HTTP 503**.

The two keep separate `reqwest::Client`s on purpose: a managed generation needs a 900s budget while
discovery calls need 30s. **This asymmetry used to be worse than a mere gap** — the managed path was
retry-less, and it holds the `managed_execution` admission permit across the upstream call, so a
bare failure also threw away a slot it had already queued for. → `references/reliability.md`

Next: backend choice/admission → `references/resource-routing.md`; symptom rather than a mode
choice → `references/troubleshooting.md`.
