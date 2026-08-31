# `freellama proxy` vs `freellama serve`

Load when choosing which mode to run, or when a `/_freellama/v1/*` call 404s and you need to know
whether that route exists at all.

Both bind `127.0.0.1:11435` by default — pick one, not both; the second fails to bind.

| | `npx freellama proxy` | `npx freellama serve` |
|---|---|---|
| Ollama passthrough (`/api/*`) | Yes, with retry/backoff/timeout | Yes, same code path — it composes `proxy::app()` as its fallback |
| `/_freellama/v1/{health,machine,models,recommendations,routes,natural-routes,sessions,tasks}` | No — 404 | Yes |
| Retry on the managed task path (`/tasks`) | n/a — route does not exist | Yes, same backoff schedule as passthrough |
| Use when | You only need a more reliable Ollama endpoint (a benchmark, a script calling `/api/chat` directly) | You want model discovery, task-aware routing, admission control, or session affinity |

Every MCP tool except `doctor`, `ollama_manage` and `ollama_delete` needs `serve`; those three talk
to Ollama directly and work without it.

## Which one am I pointed at?

- With an agent: call `doctor`. It carries a machine profile on success; `machine_unavailable`
  with a stated reason means `serve` is not up.
- Without one: `curl -sf $ENDPOINT/_freellama/v1/health` — 200 means `serve`, 404 means `proxy`.
- `scripts/check.sh` does this automatically and labels which mode is running.

## Starting either one

```bash
npx freellama proxy                                   # passthrough + retry only
npx freellama serve --policy-file platform.toml \
                    --benchmark-report bench-all.json # add these to make minConfidence "medium" reachable
```

In a checkout the npm launcher runs `target/release/freellama` if it is there, so
`cargo build --release` then `npx freellama …` works unpacked.

## Retry coverage — both paths, one schedule

Retry/backoff/timeout live in
[`proxy.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/proxy.rs)
(`send_with_retries`), and `serve`'s passthrough reuses it, so raw `/api/chat` calls through `serve`
are protected. The managed task path behind `/_freellama/v1/tasks` uses its own
`platform::post_json_with_retries` but shares `proxy::retry_delay` and `proxy::MAX_ATTEMPTS` — one
schedule, deliberately not duplicated.

The two keep separate `reqwest::Client`s on purpose: a managed generation needs a 900s budget while
discovery calls need 30s. **This asymmetry used to be worse than a mere gap** — the managed path was
retry-less, and it holds the `managed_execution` admission permit across the upstream call, so a
bare failure also threw away a slot it had already queued for. → `references/reliability.md`

Next: symptom rather than a mode choice → `references/troubleshooting.md`.
