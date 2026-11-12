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
| `GET /health` | | `doctor` (`platform_health`) | Liveness, admission capacity, and configured backends; answers as soon as serve listens |
| `GET /ready` | `freellama ready` | `doctor` (`readiness`) | 200 once the primary Ollama (and CPU backend, when configured) answers and its catalog reads; otherwise 503 with `waiting_for` |
| `GET /status` | `freellama status` | `doctor {view:"status"}` | Live view of every backend, the raw proxy, loaded models, host, Ollama settings, and today's usage |
| `GET /usage?days=N` | `freellama usage --days N` | `doctor {view:"usage", days:N}` | Totals per day and per model (default 7 days) |
| `GET /metrics` | | | Prometheus text exposition |
| `GET /config` | `freellama config` | | Effective runtime settings and the source of each |
| `POST /config/reload` | `freellama config --reload` | | Re-read the runtime file now; `422` keeps the last good values |
| `GET /jobs` | `freellama jobs` | `task_jobs {action:"list"}` | Deferred job metadata without prompts or retained results |
| `GET /jobs/:id` | `freellama jobs --id <uuid>` | `task_jobs {action:"get", jobId}` | One job receipt and its retained result or error. `?wait_ms=N` (at most 5000) holds the read until the job changes; the reply then carries `long_poll: true` |
| `POST /jobs/:id/cancel` | `freellama jobs --id <uuid> --cancel` | `task_jobs {action:"cancel", jobId}` | Cancellation receipt after local permits are released |
| `DELETE /jobs/:id` | `freellama jobs --id <uuid> --remove` | `task_jobs {action:"remove", jobId}` | Stops active work, releases local permits, and removes the retained record |

## Read the live status

`status` answers "why is my task waiting?" in one call:

- **`backends.gpu` / `backends.cpu`**: admission (units in use, current limit, ceiling, queue depth,
  oldest wait, refusal and timeout counters, and `affinity_promotions`: grants given ahead of older
  same-priority work because the waiter's model was already running or admitted last), the
  adaptive limiter state, and the circuit breaker (`closed`, `open` with seconds remaining, or
  `half_open`).
- **`raw_proxy`**: active and waiting raw streams against their cap and wait budget.
- **`loaded_models`**: Ollama's `/api/ps` with total and VRAM bytes, context length, expiry, and
  whether the model is pinned.
- **`host`**: available and reserved RAM, whether the pressure gate is holding, and discrete-GPU
  total and free memory (`nvidia-smi` or amdgpu sysfs) when available. Optional `gpu_activity`
  reports driver counters separately from resident memory. On macOS, `thermal_pressure` is the
  system thermal pressure level (`nominal`, `moderate`, `heavy`, `trapping`, `sleeping`); `heavy`
  or worse holds CPU-assigned work and cold loads. Other platforms report `null`.
- **`ollama`**: the Ollama server's effective `OLLAMA_*` settings with a source for each — the
  running process's environment (Linux `/proc/<pid>/environ`, macOS `ps eww` for a same-user
  process), `launchctl getenv` for the macOS app, FreeLlama's own environment, or Ollama's default.
  A remote endpoint is never probed and is reported as unknown.
- **`usage_today`**: tasks, errors, tokens, and busy time since midnight UTC.
- **`task_jobs`**: deferred task IDs, selected models, configured backends, priorities, total
  deadlines, states, and waiting reasons. MCP `doctor {view:"status"}` keeps only the active jobs
  plus counts by state; `task_jobs {action:"list"}` lists them all. States cover discovery, admission, resources, runner
  transitions, loading, execution, and completion. Metadata excludes the original payload.

A task receipt explains its own wait. `admission.mode` is `resident_shared` (the runner was
reused without the backend load lock) or `nonresident_transition_exclusive` (this task loaded the
runner under it). `admission.reason` says why a present runner could not be shared when its identity
was not established, for example `resident_identity_unobserved` when `/api/ps` reports no digest;
it is `null` for a plain match. `queue_wait_ms`, `transition_wait_ms` (waiting for the load lock) and
`resource_wait_ms` add up to `total_wait_ms`. Waiting for the lock is bounded by the task's queue
deadline; a raw load that waits too long gets 503 `proxy busy`, and a raw generation whose
allocation cannot be forecast gets 503 `model_forecast_unavailable`. See
[Load lock and use lease](CPU_GPU_ROUTING.md#load-lock-and-use-lease).

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
when process settings cannot be observed. Each config's `effective` block reports the resolved
`num_parallel`, `context_length`, `flash_attention` (`on`, `off`, or `auto` when unset) and the KV
cache it sizes for (`kv_cache_type`, `kv_cache_percent_of_f16`); see
[KV cache and flash attention](TUNING.md#kv-cache-and-flash-attention).

Process settings are cached for five seconds and refreshed on discovery or monitoring requests,
including after an Ollama restart. Each config receipt includes `observed_at` and
`observation_scope: "process_snapshot"`. Refresh also updates backend memory estimates and
invalidates the use of measurements taken under different parallelism or KV cache settings.
FreeLlama admission limits remain governed by its runtime configuration.

`host.gpu_activity` is nullable. When available, it identifies its `source`,
`scope: "observed_devices_all_processes"`, `window: "driver_defined"`, and individual `devices`.
Each device has an identity, nullable `busy_percent`, and nullable `memory_busy_percent`. Apple
devices may add `allocated_memory_bytes`, the driver's allocated memory across all processes.
An observed zero means the driver reported zero; a missing or malformed counter remains unknown.
Use `host.sample_age_ms` to check freshness.

NVIDIA activity comes from the same bounded `nvidia-smi` query as memory. Apple Silicon activity
comes from the accelerator's `PerformanceStatistics` in a bounded IORegistry read. Those Apple
properties are driver-specific and may disappear or change. A sample that may have reached the
64 KiB output cap or omits a recognized device's busy counter is discarded rather than treated as
complete. Malformed NVIDIA device rows likewise discard the activity sample. AMD, Windows, unsupported drivers, and unavailable reads report unknown activity. The counters include other processes, use a driver-defined observation
window, and do not attribute activity to a FreeLlama task or establish bandwidth or energy
efficiency. They are diagnostic by default. The opt-in GPU headroom guard below also uses valid
busy counters to hold new GPU work.

Each backend's admission `queue_depth` includes both `slot_waiters` and `resource_waiters`.
A resource waiter returns its execution slot after two seconds but remains in the bounded queue
until capacity recovers, its deadline expires, or it is cancelled.

Model affinity is bounded: `affinity_max_skips` (4) and `affinity_max_wait_ms` (2000) in the same
receipt are how often, and how long, an older waiter can be passed over before its class is served
in arrival order again. A rising `affinity_promotions` with flat queue waits means fewer model
switches; see [the admission design](ARCHITECTURE.md#managed-task-execution).

`GET /health` reports `admission.request_body_budget` (`total_bytes`, `available_bytes`): the
process-wide memory for request bodies being read or held by running requests,
`FREELLAMA_MAX_BUFFERED_BYTES` (256 MiB by default). When it is full, a new upload waits up to the
queue wait and is then refused with 503 `request_body_budget_exhausted`; raise it, or lower
`FREELLAMA_MAX_REQUEST_BYTES`, when large uploads are normal.

## Record usage

Every managed task, and every raw Ollama or OpenAI-compatible generation, updates in-memory usage
totals and offers one record to a bounded writer queue. The record contains time, model, backend,
task kind (`raw` for passthrough, whose `priority` field holds the path), priority, outcome, HTTP
status, prompt, cached prompt and output tokens, duration, queue wait, load time, output tokens per
second and, with client keys, the client. It contains no prompt or response text. Raw counts are read
from the fields each response reports (`prompt_eval_count`, `prompt_eval_cached_count`, `eval_count`,
OpenAI `usage`, decision `usage`) as it streams through unchanged; a client that leaves early records
what arrived. `cached_prompt_tokens` totals show how much prompt work the runners' prompt caches
saved. One worker writes records in order; task completion does not wait for disk.

The writer queue defaults to 128 records. Configure its positive startup-only capacity through
`usage_queue_capacity` or `FREELLAMA_USAGE_QUEUE_CAPACITY`; runtime reload does not resize the worker.
A full queue or unavailable worker drops the record; a disk
failure records a failed write. Inspect `usage_ledger` in `status`, or `ledger` in `usage`, for
`pending`, `failed`, `dropped`, `records`, `capacity`, and `last_error`. The durability contract is
`asynchronous_best_effort`: pending records can be lost on a crash, and in-memory totals can exceed
persisted totals. Error evidence remains visible after later successful writes.

The ledger is the `usage` table of the shared database (`~/.octocode/freellama/freellama.sqlite`). `serve`
keeps daily totals for the last 35 days in memory, computed in the database at startup, and never
deletes records. `usage --days N` past 35 days reads the older days from the database on request
(`retention.older_days: "ledger"`); with `--ephemeral` they are `"not_retained"`. Memory holds at
most 256 models per day, and the task counters at most 256 models: raw requests can name any
model, so further names are counted under `other`. Successful writes
contribute to restart totals; completion is not a zero-loss persistence promise. Embedded callers
can use the bounded `flush_ledger` barrier for orderly teardown; it confirms attempted writes, not
recovery of dropped/failed records. `--home` moves the database; `--ephemeral` keeps totals in
memory only.

## Scrape metrics

`/metrics` exposes counters (`freellama_tasks_total`, `freellama_prompt_tokens_total`,
`freellama_output_tokens_total`, `freellama_task_seconds_total`, `freellama_queue_wait_seconds_total`,
`freellama_load_seconds_total`, `freellama_raw_requests_total`, `freellama_evictions_total`,
`freellama_circuit_open_total`, `freellama_concurrency_limit_changes_total`); task counters are
labelled by model, backend, task, and outcome, raw requests by kind and outcome, and the rest by
backend. `freellama_admission_affinity_promotions_total` counts model-affinity grants per backend. Gauges cover admission, queues, the raw proxy, host and GPU memory,
loaded models, and `freellama_ollama_num_parallel`. Point Prometheus, Grafana Agent, or any
OpenMetrics-compatible scraper at it.

## Back-pressure

FreeLlama waits for safe capacity within a deadline and bounds the retained backlog:

| Situation | Status | `code` |
|---|---|---|
| Managed queue full | `429` | `admission_queue_full` |
| Raw proxy over its cap after `raw_queue_wait_seconds` | `429` | |
| Managed wait exceeded `max_queue_wait_seconds` (or the task's `max_wait_seconds`) | `503` | `admission_timeout` |
| OS pressure, swapping, CPU, thermal, or required telemetry still holds when the waiting budget expires (low available RAM alone only warns on macOS; low RAM and swapping never hold an already-resident runner, and CPU load and thermal never hold one on the GPU) | `503` | `resource_admission_unavailable` |
| Backend circuit open | `503` | `upstream_circuit_open` |

These capacity refusals carry a `Retry-After` header and `retry_after_seconds` in the body. For a queue it
is estimated from the queue depth, the lanes that can run at once (current limit divided by the
task's cost), and the backend's recent average service time, which excludes queue wait (1 to 120
seconds). A task can ask for a shorter wait with `max_wait_seconds` (`maxWaitSeconds`
in MCP); the server caps it at the configured maximum.

## Leave capacity for other applications

Resource thresholds belong to the operator, not to a model name or a fixed machine size. Start
`serve` with CPU and, optionally, GPU headroom targets:

```bash
freellama serve \
  --resource-hold-load-per-cpu 0.75 \
  --resource-resume-load-per-cpu 0.5 \
  --resource-hold-gpu-busy-percent 80 \
  --resource-resume-gpu-busy-percent 60
```

These are **admission targets, not utilization quotas**. They hold new work until observations
recover; they do not pause, kill, or throttle work already executing. One admitted inference can
still saturate a device. Use OS/container CPU quotas and upstream/device isolation when a hard
resource partition is required. Direct calls to Ollama bypass FreeLlama's guard.

| Setting | Default | Meaning |
|---|---|---|
| `--resource-hold-load-per-cpu` | `1.5` | Hold CPU-bound work when one-minute load / logical CPUs reaches this value |
| `--resource-resume-load-per-cpu` | `1.0` | Recover the CPU hold at or below this value |
| `--resource-hold-gpu-busy-percent` | Disabled | Hold GPU work when any observed device reaches this busy percentage |
| `--resource-resume-gpu-busy-percent` | Disabled | Recover only when all observed devices are at or below this percentage |

The four flags also accept environment variables named `FREELLAMA_RESOURCE_HOLD_LOAD_PER_CPU`,
`FREELLAMA_RESOURCE_RESUME_LOAD_PER_CPU`, `FREELLAMA_RESOURCE_HOLD_GPU_BUSY_PERCENT`, and
`FREELLAMA_RESOURCE_RESUME_GPU_BUSY_PERCENT`. They are startup settings, not runtime-TOML keys;
changing them requires restarting your own FreeLlama service. Inspect the effective policy under
`host.policy` in raw `status` (or the full saved `doctor` response).

CPU load is **not CPU utilization**: `0.75` does not guarantee 25% idle CPU. It scales with logical
CPU count and includes the OS's runnable/waiting load. Both CPU thresholds must be finite and
positive, with resume less than hold; no fixed upper ceiling is imposed. Missing CPU telemetry
follows `--resource-telemetry-policy`; use `require-all` if unavailable CPU-load observations must
block work. CPU load holds CPU-assigned work and work on hosts without an observed accelerator;
it does not hold an observed GPU runner merely because a compiler is busy. Raw Ollama requests
with explicit integer `options.num_gpu: 0` use CPU admission without rewriting the body; this is
request intent, not proof of physical placement. Unknown raw/OpenAI-compatible placement stays
conservatively GPU-gated when the optional GPU guard is enabled.

Both GPU settings must be supplied together and satisfy `0 <= resume < hold <= 100`. GPU activity
is optional by default so unsupported machines continue to work. When explicitly enabled, missing,
partial, or invalid GPU busy counters refuse GPU admission with `gpu_activity` in the assessment's
`missing_telemetry`, even with `best-effort`; CPU-assigned work remains eligible. Enable this guard
only on hosts exposing the counters described above. It conservatively considers the busiest of
all observed GPUs, not a per-model device assignment. CPU-only or remote Ollama deployments should
enforce resource controls on the machine actually executing work: remote upstreams bypass local
resource admission. Holds recover after two fresh healthy samples by default, avoiding rapid
stop/start at a single threshold.

For memory headroom, the existing `--resource-hold-available-percent` and
`--resource-resume-available-percent` settings default to 15 and 20. The per-load reserve is
size-aware rather than a promise to keep that entire share unused for every small model. Use
`model_memory_budget` in the runtime file to cap combined model residency on unified-memory/CPU
backends. GPU busy percentage is not a VRAM reserve.

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
costs, and execution receipts use the resolved policy. Health exposes the complete task-kind map
under `admission.costs.base_by_task` and the embedding batch formula. Task kinds remain
caller-declared; these weights are cooperative scheduling policy, not workload classification.
See the
[runtime example](https://github.com/bgauryy/FreeLlama/blob/main/freellama.runtime.example.toml) for the table syntax.

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
`adaptive.profile_windows` keyed by task. Embedding windows report their explicit input-rate kind
separately. Use the task windows for learning evidence. See [automatic feedback](CPU_GPU_ROUTING.md#understand-automatic-feedback) for
persisted routing windows.

The CPU backend is adaptive by default because oversubscribing it shows up as a throughput collapse
rather than an error. Every change is counted in metrics and shown in `status`.

## Model residency and eviction

Omitted managed `keep_alive` values use finite retention informed by measured reuse and loading,
starting from `base_seconds` (300, Ollama's own default) and shortened by `pressure_factor` under a
host hold. Explicit values take precedence. The background pressure sweep releases, under
FreeLlama's own low-memory estimate, only runners idle longer than `base_seconds ×
pressure_factor` (75 s by default); swapping or OS memory pressure releases any idle runner. For governed prewarming, profile compatibility, and retention
settings, see [Scope history and model warming](SCOPES_AND_WARMING.md).

When a model about to load needs room, FreeLlama unloads idle runners itself (`keep_alive: 0`)
instead of letting the load wait, fail, or spill to CPU. It does this in four cases, checked in
this order: (discrete GPU) the model does not fit in free VRAM, where Ollama otherwise chooses its
own victim; the load would take the backend's loaded models past `model_memory_budget`; host RAM
can fall below the reserve; or the host is already holding for low memory. Raw passthrough loads
(an Ollama API call for a model that is not loaded) take the same steps, and raw calls in flight
count as busy, so their models are not unloaded under them.

`model_memory_budget` (runtime file, or `FREELLAMA_MODEL_MEMORY_BUDGET`) is `"75%"` of RAM by
default; set a size such as `"24GiB"`, another share, or `"off"`. It applies where the runners share
host memory: unified memory (Apple silicon) or a CPU backend.

Which runners go is a small optimisation, in the spirit of llama-swap's `evict_costs` and LocalAI's
busy-aware LRU:

- **Never unloaded:** the model being loaded, `pinned_models`, `keep_alive: -1` runners, runners
  Ollama is still loading, and any model with a FreeLlama task queued or running (unloading it forces an immediate reload). Busy status is checked again right before each unload.
- **Not for a model bigger than the GPU:** it spills to the CPU whatever is unloaded, so VRAM is
  left alone.
- **Cost of unloading a runner** = its reload time (Ollama's measured `load_duration` from earlier
  loads, kept in the shared database across restarts, or size ÷ 1.5 GB/s before the first one) × (1 + recent uses, decaying with a 30-minute
  half-life; only tasks served count) × its `eviction_costs` weight (default 1).
- **Choice:** the set of runners with the lowest total cost that frees enough memory; ties go to
  fewer runners, then less memory freed beyond what is needed. Only the memory that matters is
  counted: VRAM for a GPU fit, the host-RAM part for a spill, the whole runner on CPU or unified
  memory.

Every managed receipt that caused an eviction carries the plan under
`memory_reservation.eviction`: what was unloaded, its cost and reload estimate, what was kept and
why, the `trigger` (`gpu_memory`, `model_memory_budget` or `host_memory`) and `unload_ms`. `status`
and the page show the latest one that unloaded something. Eviction runs once per task, after
admission and before the memory wait, and is never cancelled halfway by a deadline. Set
`evict_idle_models = false` to leave residency entirely to Ollama.

`memory_reservation` names the `kv_cache_type` and `flash_attention` state it sized for. With a
quantized cache it also carries `ollama_f16_preflight_bytes`, the f16-cache size Ollama's own fit
check predicts. On a CPU or
unified-memory backend, `ollama_preflight_threshold_bytes` is 80% of the available memory the load
was admitted against; when the reserved estimate fits under it and the f16 figure does not, the
receipt adds `ollama_may_evict_or_offload: true` and a one-line `ollama_preflight_reason`: Ollama may
unload other runners or offload layers even though FreeLlama found room. Run previews carry the
same fields. The reservation itself is unchanged: measurements still take precedence over the
estimate.

### The residency audit

Every load, unload and eviction is recorded with its cause and duration in the shared database, so
the audit covers every FreeLlama process on the machine. `status` summarizes the last hour under
`residency`:

| Field | Meaning |
|---|---|
| `model_memory_budget` | The setting, its bytes, and per backend whether it applies, what its loaded models hold, and the share used |
| `audit.counts`, `audit.by_trigger` | Loads, unloads and evictions; causes such as `managed_task`, `raw_request`, `caller_unload`, `model_memory_budget`, `host_memory` |
| `audit.switch_ms` | Per model: measured loads, average, total and last load time (Ollama's `load_duration`) |
| `audit.thrashing` | Models loaded 3 or more times in 10 minutes: raise the budget, pin the model, or give it its own backend |
| `audit.recent` | The 20 newest events (the database keeps every event; counts cover the whole hour) |

A raw passthrough load is recorded when its response ends, with the `load_duration` Ollama
reported in it, and that time also teaches the eviction planner what reloading the model costs.
Preloads appear as `preload` or `preload_failed` events with trigger `startup` or
`predicted_after:<model>`.

A task refused because the host stayed under pressure says which signals held it:
`resource_admission.held_by` (such as `["thermal_throttling"]`), also named in the error text.

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
including ones loaded by raw clients, and saves those measurements in the shared database, keeping
the larger size when two processes measure the same runner. After a restart a model it has seen before is sized from Ollama's measurement, not a
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
`auto_context_max` (32768). The default is the Modelfile's `num_ctx`, then the
runtime file's `ollama_default_context`, then `OLLAMA_CONTEXT_LENGTH`, then (primary backend
on a discrete GPU only) Ollama's VRAM tier, from total VRAM minus `OLLAMA_GPU_OVERHEAD` — 4096 below
23 GiB, 32768 below 47 GiB, 262144 from 47 GiB. Receipts show `context_sizing.num_ctx_sent` and the default used.
Memory estimates always use the context Ollama can allocate. Use `"explicit"` to always
send `num_ctx`.

The context-shift setting is part of a runner's identity too: Ollama reloads a runner when a
request's `shift` differs from the one it was loaded with, and its default is on for most models.
`context_shift = "off"` (the default) sends `shift: false` on managed chat, so a generation that
reaches the context limit stops instead of discarding its earliest tokens, at the cost of a reload
when raw clients use the same model. `"ollama_default"` omits `shift` so raw and managed chat share
the runner. Both modes send `truncate: false`. `status` reports it under `ollama.context_shift`.

## Change settings without a restart

Put any of the settings above in a TOML file and start `serve --runtime-config <file>` (or set
`FREELLAMA_RUNTIME_CONFIG`). Start from
[`freellama.runtime.example.toml`](https://github.com/bgauryy/FreeLlama/blob/main/freellama.runtime.example.toml). `serve` checks the file every
two seconds and applies changes to live admission pools; a waiting task sees the new limit at once.

Precedence for every setting, strongest first: CLI flag, environment variable (where one exists),
the runtime file, the built-in default. Only these settings have an environment variable:
`FREELLAMA_MAX_QUEUE_WAIT_SECONDS`, `FREELLAMA_EVICT_IDLE_MODELS`, `FREELLAMA_USAGE_QUEUE_CAPACITY`,
and the individual `FREELLAMA_SCOPE_*` and `FREELLAMA_KEEP_ALIVE_*` limits. Set every other key in
the runtime file or with its CLI flag. A flag or variable pins a value, so editing the file cannot
override it; `freellama config` shows which source won: `cli`, `env`, `file`, a default source
such as `default` or `ollama_num_parallel`, or `env_invalid` when a set variable failed validation.
A malformed file or an unknown key is rejected at reload and the previous values stay in force.

The default GPU budget follows the Ollama server: 2 units per `OLLAMA_NUM_PARALLEL` slot (one chat
per decoding slot), read from the running Ollama process where visible.
