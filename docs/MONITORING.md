# Monitoring and live tuning

`freellama serve` reports what it is doing, records what it did, and accepts new limits without a
restart. Everything on this page is served under `/_freellama/v1/` and needs the same bearer token
as the rest of the control API when auth is enabled.

| Endpoint | CLI | MCP | What it returns |
|---|---|---|---|
| `GET /status` | `freellama status` | `doctor {view:"status"}` | Live view of every backend, the raw proxy, loaded models, host, Ollama settings, and today's usage |
| `GET /usage?days=N` | `freellama usage --days N` | `doctor {view:"usage", days:N}` | Totals per day and per model (default 7 days) |
| `GET /metrics` | | | Prometheus text exposition |
| `GET /config` | `freellama config` | | Effective runtime settings and the source of each |
| `POST /config/reload` | `freellama config --reload` | | Re-read the runtime file now; `422` keeps the last good values |

## Read the live status

`status` answers "why is my task waiting?" in one call:

- **`backends.gpu` / `backends.cpu`**: admission (units in use, current limit, ceiling, queue depth,
  oldest wait, refusal and timeout counters), the adaptive limiter state, and the circuit breaker
  (`closed`, `open` with seconds remaining, or `half_open`).
- **`raw_proxy`**: active and waiting raw streams against their cap and wait budget.
- **`loaded_models`**: Ollama's `/api/ps` with total and VRAM bytes, context length, expiry, and
  whether the model is pinned.
- **`host`**: available and reserved RAM, whether the pressure gate is holding, and discrete-GPU
  total and free memory (`nvidia-smi` or amdgpu sysfs) when available.
- **`ollama`**: the Ollama server's effective `OLLAMA_*` settings with a source for each — the
  running process's environment (Linux `/proc/<pid>/environ`, macOS `ps eww` for a same-user
  process), `launchctl getenv` for the macOS app, FreeLlama's own environment, or Ollama's default.
  A remote endpoint is never probed and is reported as unknown.
- **`usage_today`**: tasks, errors, tokens, and busy time since midnight UTC.

## Record usage

Every managed task appends one JSON line to the usage ledger: time, model, backend, task kind,
priority, outcome, HTTP status, prompt and output tokens, duration, queue wait, load time, and
output tokens per second. No prompt or response text is stored. `serve` replays the ledger at
startup so daily totals survive a restart, and rotates it to `usage.jsonl.1` at 16 MB.

The ledger lives next to the feedback file in the platform data directory. Choose another path with
`--usage-file` (or `FREELLAMA_USAGE_FILE`), or keep totals in memory only with `--ephemeral-usage`.

## Scrape metrics

`/metrics` exposes counters (`freellama_tasks_total`, `freellama_prompt_tokens_total`,
`freellama_output_tokens_total`, `freellama_task_seconds_total`, `freellama_queue_wait_seconds_total`,
`freellama_load_seconds_total`, `freellama_raw_requests_total`, `freellama_evictions_total`,
`freellama_circuit_open_total`, `freellama_concurrency_limit_changes_total`) labelled by model,
backend, task, and outcome, plus gauges for admission, queues, the raw proxy, host and GPU memory,
loaded models, and `freellama_ollama_num_parallel`. Point Prometheus, Grafana Agent, or any
OpenMetrics-compatible scraper at it.

## Back-pressure

FreeLlama refuses work it cannot start soon instead of letting it pile up inside Ollama:

| Situation | Status | `code` |
|---|---|---|
| Managed queue full | `429` | `admission_queue_full` |
| Raw proxy over its cap after `raw_queue_wait_seconds` | `429` | |
| Managed wait exceeded `max_queue_wait_seconds` (or the task's `max_wait_seconds`) | `503` | `admission_timeout` |
| Host memory held by the pressure gate | `503` | `resource_admission_unavailable` |
| Backend circuit open | `503` | `upstream_circuit_open` |

Every refusal carries a `Retry-After` header and `retry_after_seconds` in the body. For a queue it
is estimated from the queue depth, the current limit, and the backend's recent average task time
(1 to 120 seconds). A task can ask for a shorter wait with `max_wait_seconds`; it is capped by the
configured maximum.

## Circuit breaker

After `breaker_failures` consecutive upstream failures on a backend (connection errors, `500`,
`502`, or `504`), its circuit opens for `breaker_cooldown_seconds`; tasks routed there fail fast
with `503` rather than queueing behind a broken Ollama. When the cooldown ends, one probe request is
let through: success closes the circuit, failure reopens it. `4xx` responses such as an unknown
model do not count. Set `breaker_failures = 0` to disable it.

## Adaptive concurrency

With `adaptive_concurrency = "cpu"` (the default) or `"all"`, each pool's limit moves between 1 and
its configured ceiling:

- **Down (halve)** on an upstream error, host memory pressure (PSI or the pressure gate), or output
  throughput below 70% of that model's own baseline. Decreases are at least 10 seconds apart.
- **Up (+1 unit)** after five consecutive healthy completions.

The CPU backend is adaptive by default because oversubscribing it shows up as a throughput collapse
rather than an error. Every change is counted in metrics and shown in `status`.

## Model residency

Before loading a model that needs memory, FreeLlama unloads idle resident models (`keep_alive: 0`)
that no admitted task is using. Models in `pinned_models` are never unloaded this way; set
`evict_idle_models = false` to leave residency entirely to Ollama.

## Context sizing and Ollama's defaults

Sending `num_ctx` on a request pins the runner to that size: a later request for the same model
with a different or missing `num_ctx` forces Ollama to reload it, and Ollama's automatic
out-of-memory shrink-and-retry only applies when the context was not requested explicitly.

With `context_mode = "ollama_default"` (the default), FreeLlama sizes the context as before but
omits `num_ctx` when Ollama's own default for the model is known, covers the request, and is at most
`auto_context_max` (32768). The default is resolved in Ollama's order: the Modelfile's `num_ctx`,
then `OLLAMA_CONTEXT_LENGTH`, then (discrete GPU only) Ollama's VRAM tier — 4096 below 24 GiB, 32768
below 48 GiB, 262144 above. Receipts show `context_sizing.num_ctx_sent` and the default used.
Memory estimates always use the context Ollama will actually allocate. Use `"explicit"` to always
send `num_ctx`.

## Change settings without a restart

Put any of the settings above in a TOML file and start `serve --runtime-config <file>` (or set
`FREELLAMA_RUNTIME_CONFIG`). Start from
[`freellama.runtime.example.toml`](../freellama.runtime.example.toml). `serve` checks the file every
two seconds and applies changes to live admission pools; a waiting task sees the new limit at once.

Precedence for every setting, strongest first: CLI flag, `FREELLAMA_*` environment variable, the
runtime file, the built-in default. A flag or variable pins a value, so editing the file cannot
override it; `freellama config` shows which source won. A malformed file or an unknown key is
rejected at reload and the previous values stay in force.

The default GPU budget follows the Ollama server: 2 units per `OLLAMA_NUM_PARALLEL` slot (one chat
per decoding slot), read from the running Ollama process where visible.
