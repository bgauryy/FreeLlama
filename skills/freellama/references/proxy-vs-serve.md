# `freellama proxy` vs `freellama serve`

Load when choosing which mode to run, or when a `/_freellama/v1/*` call returns 404 and you need
to know whether that route exists at all.

Both bind `127.0.0.1:11435` by default (pick one, not both — the second will fail to bind).

| | `freellama proxy` | `freellama serve` |
|---|---|---|
| Passthrough to Ollama | Yes, with retry/backoff/timeout | Yes, same code path (composes `proxy::app()` as its fallback) |
| `/_freellama/v1/{machine,models,routes,natural-routes,sessions,tasks}` | No — 404 | Yes |
| Use when | You only need a more reliable Ollama endpoint (a benchmark, a script calling `/api/chat` directly) | You want model discovery, task-aware routing, or session affinity |

Verify which one you're pointed at: call `doctor` (it carries the machine profile) — success means `serve`, a
connection/404 error means `proxy` — or `curl -sf $ENDPOINT/_freellama/v1/machine` without an
agent (200 means `serve`, 404 means `proxy`). `scripts/check.sh` does this automatically.

## The one gap this doesn't cover

The retry/backoff/timeout logic lives entirely in `packages/rust-core/src/proxy.rs` (`send_with_retries`). `serve`'s
passthrough route reuses it (`platform/mod.rs` composes `proxy::app()` as its fallback), so raw
`/api/chat` calls through `serve` ARE protected. But `forward_managed_task` in `packages/rust-core/src/platform/mod.rs`
(behind `/_freellama/v1/tasks`) builds its own separate `reqwest::Client` and does **not** call
through `send_with_retries` — managed-task requests get no retry protection today. If you rely on
`/tasks` under a flaky Ollama, this is the first place to look; it hasn't been fixed because nothing
in this repo currently exercises that path under load.

## Starting either one

```bash
cargo build --release
./target/release/freellama proxy    # or: cargo run --release -- proxy
./target/release/freellama serve --recommendation-catalog recommendations.example.toml
```

`benchmark/local/scripts/restart_ollama.sh` starts `proxy` mode specifically, because the benchmark
adapters only need `/api/chat` passthrough — they pick their own model per adapter and don't need
routing.
