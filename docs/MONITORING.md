# Runtime status and live tuning

Use runtime status to inspect how FreeLlama manages delegated work: per-backend queues and
admission, memory pressure, resident models, execution usage, and resource holds. The operator can
reload supported limits without a restart; the calling agent can use these observations to decide
whether to wait, narrow a task, or handle it itself.

The API endpoints in this reference are served under `/_freellama/v1/` and use the same bearer
token as the rest of the control API when authentication is enabled.

Open **`http://127.0.0.1:11435/_freellama/ui`** (the address `serve` prints at startup) for a live
page of the same data, refreshed every two seconds: backends and queues, raw traffic, host RAM and
VRAM, Ollama's settings, loaded models with their current tasks and load times, and the last
eviction. The page itself is static and needs no token; when auth is on it asks for the bearer
token and keeps it only for the browser tab.

| Endpoint | CLI | MCP | What it returns |
|---|---|---|---|
| `GET /status` | `freellama status` | `doctor {view:"status"}` | Live view of every backend, the raw proxy, loaded models, host, Ollama settings, and today's usage |
| `GET /usage?days=N` | `freellama usage --days N` | `doctor {view:"usage", days:N}` | Totals per day and per model (default 7 days) |
| `GET /metrics` | | | Prometheus text exposition |
| `GET /config` | `freellama config` | | Effective runtime settings and the source of each |
| `POST /config/reload` | `freellama config --reload` | | Re-read the runtime file now; `422` keeps the last good values |
| `GET /jobs` | `freellama jobs` | `task_jobs {action:"list"}` | Deferred job metadata without prompts or retained results |
| `GET /jobs/:id` | `freellama jobs --id <uuid>` | `task_jobs {action:"get", jobId}` | One job receipt and its retained result or error |
| `POST /jobs/:id/cancel` | `freellama jobs --id <uuid> --cancel` | `task_jobs {action:"cancel", jobId}` | Cancellation receipt after local permits are released |
| `DELETE /jobs/:id` | `freellama jobs --id <uuid> --remove` | `task_jobs {action:"remove", jobId}` | Stops active work, releases local permits, and removes the retained record |

## Read the live status

`status` answers "why is my task waiting?" in one call:

- **`backends.gpu` / `backends.cpu`**: admission (units in use, current limit, ceiling, queue depth,
  oldest wait, refusal and timeout counters), the adaptive limiter state, and the circuit breaker
  (`closed`, `open` with seconds remaining, or `half_open`).
- **`raw_proxy`**: active and waiting raw streams against their cap and wait budget.
- **`loaded_models`**: Ollama's `/api/ps` with total and VRAM bytes, context length, expiry, and
  whether the model is pinned.
- **`host`**: available and reserved RAM, whether the pressure gate is holding, and discrete-GPU
  total and free memory (`nvidia-smi` or amdgpu sysfs) when available. Optional `gpu_activity`
  reports driver counters separately from resident memory.
- **`ollama`**: the Ollama server's effective `OLLAMA_*` settings with a source for each — the
  running process's environment (Linux `/proc/<pid>/environ`, macOS `ps eww` for a same-user
  process), `launchctl getenv` for the macOS app, FreeLlama's own environment, or Ollama's default.
  A remote endpoint is never probed and is reported as unknown.
- **`usage_today`**: tasks, errors, tokens, and busy time since midnight UTC.
- **`task_jobs`**: deferred task IDs, selected models, configured backends, priorities, total
  deadlines, states, and waiting reasons. States cover discovery, admission, resources, runner
  transitions, loading, execution, and completion. Metadata excludes the original payload.

Use `run_task {defer:true}` in MCP or `task --defer` in the CLI to get a job handle. A list or
status request omits results; fetching one ID returns its retained result or structured failure.
Terminal states are `completed`, `failed`, `cancelled`, and `expired`. Cancellation is per task,
waits for local permits to be released, and does not unload a shared runner. Process-local job
retention limits are documented in [CLI controls](CLI.md#execute-tasks).

Removal uses the same exact job ID. It cancels active work before discarding the record, returns
`{id, removed:true, status, scope:"process_memory"}`, and omits the saved result. The ID then returns
`404` on lookup or another removal. Other jobs and installed models are unaffected. Local
cancellation closes the upstream request; physical computation may continue on a shared runner.

`ollama.config` describes the primary process; `ollama.cpu_config` describes the CPU process when
configured. Process attribution matches `OLLAMA_HOST` to the selected endpoint, so separate
parallelism and KV cache settings size each backend independently. Fallback sources remain visible
when process settings cannot be observed.

Process settings are cached for five seconds and refreshed on discovery or monitoring requests,
including after an Ollama restart. Each config receipt includes `observed_at` and
`observation_scope: "process_snapshot"`. Refresh also updates backend memory estimates and
invalidates the use of measurements taken under different parallelism or KV cache settings.
FreeLlama admission limits remain governed by its runtime configuration.

`host.gpu_activity` is nullable. When available, it identifies its `source`,
`scope: "observed_devices_all_processes"`, `window: "driver_defined"`, and individual `devices`.
Each device has an identity, nullable `busy_percent`, and nullable `memory_busy_percent`.
An observed zero means the driver reported zero; a missing or malformed counter remains unknown.
Use `host.sample_age_ms` to check freshness.

NVIDIA activity comes from the same bounded `nvidia-smi` query as memory. Apple Silicon activity
comes from the accelerator's `PerformanceStatistics` in a bounded IORegistry read. Those Apple
properties are driver-specific and may disappear or change; the read can cover only the devices
within its retained output. AMD, Windows, unsupported drivers, and unavailable reads report unknown activity. The counters include other processes, use a driver-defined observation
window, and do not attribute activity to a FreeLlama task or establish bandwidth or energy
efficiency. They are diagnostics and do not alter admission or pressure decisions.

Each backend's admission `queue_depth` includes both `slot_waiters` and `resource_waiters`.
A resource waiter returns its execution slot after two seconds but remains in the bounded queue
until capacity recovers, its deadline expires, or it is cancelled.

## Record usage

Every managed task updates in-memory usage totals and offers one record to a bounded writer queue.
The record contains time, model, backend, task kind, priority, outcome, HTTP status, prompt and
output tokens, duration, queue wait, load time, and output tokens per second. It contains no prompt
or response text. One worker writes records in order; task completion does not wait for disk.

The writer queue defaults to 128 records. Configure its positive startup-only capacity through
`usage_queue_capacity` or `FREELLAMA_USAGE_QUEUE_CAPACITY`; runtime reload does not resize the worker.
A full queue or unavailable worker drops the record; a disk
failure records a failed write. Inspect `usage_ledger` in `status`, or `ledger` in `usage`, for
`pending`, `failed`, `dropped`, `records`, `capacity`, and `last_error`. The durability contract is
`asynchronous_best_effort`: pending records can be lost on a crash, and in-memory totals can exceed
persisted totals. Error evidence remains visible after later successful writes.

`serve` replays a bounded regular-file ledger at startup and rotates it to `usage.jsonl.1` at
16 MiB. Successful writes contribute to restart totals; completion is not a zero-loss persistence
promise. Embedded callers can use the bounded `flush_ledger` barrier for orderly teardown; it
confirms attempted writes, not recovery of dropped/failed records or a filesystem sync.

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

FreeLlama waits for safe capacity within a deadline and bounds the retained backlog:

| Situation | Status | `code` |
|---|---|---|
| Managed queue full | `429` | `admission_queue_full` |
| Raw proxy over its cap after `raw_queue_wait_seconds` | `429` | |
| Managed wait exceeded `max_queue_wait_seconds` (or the task's `max_wait_seconds`) | `503` | `admission_timeout` |
| Host resources remain held when the waiting budget expires | `503` | `resource_admission_unavailable` |
| Backend circuit open | `503` | `upstream_circuit_open` |

These capacity refusals carry a `Retry-After` header and `retry_after_seconds` in the body. For a queue it
is estimated from the queue depth, the current limit, and the backend's recent average task time
(1 to 120 seconds). A task can ask for a shorter wait with `max_wait_seconds` (`maxWaitSeconds`
in MCP); the server caps it at the configured maximum.

## Circuit breaker

After `breaker_failures` consecutive upstream failures on a backend (connection errors, `500`,
`502`, or `504`), its circuit opens for `breaker_cooldown_seconds`; tasks routed there fail fast
with `503` rather than queueing behind a broken Ollama. When the cooldown ends, one probe request is
let through: success closes the circuit, failure reopens it. `4xx` responses such as an unknown
model do not count. Set `breaker_failures = 0` to disable it.

## Adaptive concurrency

Managed task costs are configurable through the runtime file's `[task_costs]` table. Valid keys
are `completion`, `coding`, `code_repair`, `tools`, `browser`, `vision`, `embedding`, and
`long_context`. Each value is a positive integer through 4294967295. Omitted kinds retain their
defaults: vision four, embeddings one, and other tasks two. Embeddings multiply their base by
`ceil(input_items/4)`; the acquired charge cannot exceed the pool's current limit.

For a qualified light OCR workload, `[task_costs]` with `vision = 2` permits two vision requests
under a four-unit budget while leaving other task costs unchanged. This is an operator setting,
not automatic quality or fit qualification. The memory governor and Ollama's final admission
still apply.

A task captures its cost when execution starts. Reload affects tasks that start afterward;
already queued or active tasks keep their captured cost. `config` reports effective values and
the runtime-file source; invalid keys, zero,
or overflow reject the reload and retain the last good configuration. Preview advice, health
costs, and execution receipts use the resolved policy. Health retains its summary aliases and
exposes the complete task-kind map under `admission.costs.base_by_task`. Task kinds remain
caller-declared; these weights are cooperative scheduling policy, not workload classification.
See the
[runtime example](../freellama.runtime.example.toml) for the table syntax.

With `adaptive_concurrency = "cpu"` (the default) or `"all"`, each pool's limit moves between 1 and
its configured ceiling:

- **Down (halve)** on an upstream error, host memory pressure (PSI or the pressure gate), or output
  throughput below 70% of the matching execution profile's baseline. Decreases are at least
  10 seconds apart.
- **Up (+1 unit)** after five consecutive comparable healthy rate observations. Generation uses
  output tokens per decode second; embeddings use input tokens per model-reported total second.
  The profile records the metric kind, and unlike kinds cannot train or recover together.

Only completed warm executions with verified placement and a comparable observed profile train
speed baselines or healthy recovery. Identity includes model digest, explicit observed context,
backend/process settings, effective controls, and admission class. Fresh endpoint-attributed
process observations bracket candidate serial samples. Unknown identity, changed controls,
parallel intervals, invalid rate observations, and incomplete or canceled execution break the
recovery streak. Failures and observed memory pressure retain their safety decreases.

Serial samples require one managed task throughout the interval: unchanged admitted/released
counters, active charge, capacity, and policy epoch reject transient overlap or capacity changes.
This avoids comparing serial request speed with legitimate parallel decoding. It does not prove
that no external Ollama client ran concurrently. A successful parallel execution can therefore
report verified placement and `feedback.accepted:true` while
`execution.throughput_learning.eligible:false`; it contributes no comparable speed sample.

Each backend retains at most one adaptive profile window per task kind. Status exposes
`adaptive.profile_windows` keyed by task; `baseline_output_tokens_per_second` is a compatibility
output-only summary by model and can omit additional task windows for the same model. Embedding
windows report their explicit input-rate kind separately. Use the task windows for learning
evidence. See [automatic feedback](CPU_GPU_ROUTING.md#understand-automatic-feedback) for
persisted routing windows.

The CPU backend is adaptive by default because oversubscribing it shows up as a throughput collapse
rather than an error. Every change is counted in metrics and shown in `status`.

## Model residency and eviction

Omitted managed `keep_alive` values use finite retention informed by measured reuse and loading.
Explicit values take precedence. For governed prewarming, profile compatibility, and retention
settings, see [Scope history and model warming](SCOPES_AND_WARMING.md).

When a model about to load needs room, FreeLlama unloads idle runners itself (`keep_alive: 0`)
instead of letting the load wait, fail, or spill to CPU. It does this in three cases: host RAM
can fall below the reserve, the host is already holding for low memory, or (discrete GPU) the
model does not fit in free VRAM, where Ollama otherwise chooses its own victim.

Which runners go is a small optimisation, in the spirit of llama-swap's `evict_costs` and LocalAI's
busy-aware LRU:

- **Never unloaded:** the model being loaded, `pinned_models`, `keep_alive: -1` runners, runners
  Ollama is still loading, and any model with a FreeLlama task queued or running (unloading it forces an immediate reload). Busy status is checked again right before each unload.
- **Not for a model bigger than the GPU:** it spills to the CPU whatever is unloaded, so VRAM is
  left alone.
- **Cost of unloading a runner** = its reload time (Ollama's measured `load_duration` from earlier
  loads, or size ÷ 1.5 GB/s before the first one) × (1 + recent uses, decaying with a 30-minute
  half-life; only tasks served count) × its `eviction_costs` weight (default 1).
- **Choice:** the set of runners with the lowest total cost that frees enough memory; ties go to
  fewer runners, then less memory freed beyond what is needed. Only the memory that matters is
  counted: VRAM for a GPU fit, the host-RAM part for a spill, the whole runner on CPU or unified
  memory.

Every managed receipt that caused an eviction carries the plan under
`memory_reservation.eviction`: what was unloaded, its cost and reload estimate, and what was kept
and why. `status` and the page show the latest one that unloaded something. Eviction runs once per
task, after admission and before the memory wait, and is never cancelled halfway by a deadline.
Set `evict_idle_models = false` to leave residency entirely to Ollama.

## Who decides what: FreeLlama and Ollama

Two schedulers that both guess at memory can disagree. The split is:

| Decision | Owner | How |
|---|---|---|
| Exact memory of a loaded runner | Ollama | Measured; FreeLlama reads it from `/api/ps` |
| Whether a new load fits | Both, Ollama final | FreeLlama estimates from Ollama's past measurements, then Ollama loads or refuses |
| Decoding slots per model | Ollama | `OLLAMA_NUM_PARALLEL`; FreeLlama's primary budget defaults to two weighted units per slot |
| Admission, priority, queue limits, back-pressure | FreeLlama | Weighted pools, 429/503 with `Retry-After` |
| Which idle runner to unload, and when | FreeLlama | The cost planner above, before Ollama has to choose |
| Context size | Ollama by default | `num_ctx` is left to Ollama when its default covers the request |

To keep the estimates honest, FreeLlama learns from every runner Ollama reports as resident,
including ones loaded by raw clients, and saves those measurements to `footprints.json` next to the
usage ledger. After a restart a model it has seen before is sized from Ollama's measurement, not a
formula. Each measurement records the `OLLAMA_NUM_PARALLEL` and KV-cache type it was taken under and
is used only while those match, so changing either setting falls back to the formula until Ollama
measures again. A larger context than any measured one is sized as the measurement plus exactly the extra
KV cache, when the model's KV shape is known.

## Context sizing and Ollama's defaults

Sending `num_ctx` on a request pins the runner to that size: a later request for the same model
with a different or missing `num_ctx` forces Ollama to reload it, and Ollama's automatic
out-of-memory shrink-and-retry only applies when the context was not requested explicitly.

With `context_mode = "ollama_default"` (the default), FreeLlama sizes the context as before but
omits `num_ctx` when Ollama's own default for the model is known, covers the request, and is at most
`auto_context_max` (32768). The default is resolved in Ollama's order: the Modelfile's `num_ctx`,
then `OLLAMA_CONTEXT_LENGTH`, then (discrete GPU only) Ollama's VRAM tier — 4096 below 24 GiB, 32768
below 48 GiB, 262144 above. Receipts show `context_sizing.num_ctx_sent` and the default used.
Memory estimates always use the context Ollama can allocate. Use `"explicit"` to always
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
