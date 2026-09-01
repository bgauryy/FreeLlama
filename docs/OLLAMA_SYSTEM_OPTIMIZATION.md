# Ollama and FreeLlama optimization boundary

Ollama already owns inference optimization. FreeLlama can improve completed
work per hour by choosing an eligible model and avoiding unnecessary model
changes, but it does not make an individual model decode faster.

This page explains the active request flow, the settings Ollama owns, the
current machine audit, and the optimization work that remains.

The architecture is portable across Ollama's Metal, CUDA, ROCm, Vulkan, and CPU paths. The audit
later on this page is intentionally Mac-specific. FreeLlama discovers host RAM per operating system
and keeps device selection in each Ollama process; it does not translate one machine's memory tier,
core count, or benchmark into another machine's defaults.

## Decision

Use stock Ollama as the inference runtime. Keep FreeLlama as an optional Rust
control plane for evidence-qualified routing, bounded request profiles, session
affinity, and policy enforcement.

Do not advertise FreeLlama as an accelerator or an Ollama inference plugin. It does not tune
kernels, split one model's layers across CPU and GPU, manage macOS memory pressure, or improve that
model's tokens per second. It can assign different models to isolated CPU and GPU-capable Ollama
processes, allowing independent workloads to overlap.

## Request flow

FreeLlama has four distinct paths:

| Request | FreeLlama work | Ollama work | Result |
|---|---|---|---|
| Native `/api/*` or `/v1/*` | Streams bytes to the primary compatibility upstream | Performs normal inference and scheduling | Exact Ollama response |
| `POST /_freellama/v1/routes` | Discovers models, filters capabilities, and applies policy | Supplies installed-model and residency data | Model and request profile; no inference |
| `POST /_freellama/v1/natural-routes` | Normalizes a schema-bound intent, then runs the deterministic router | Runs the small intent model | Model and request profile; selected task does not run |
| `POST /_freellama/v1/tasks` | Selects a route and backend, applies managed-task admission, and constructs one bounded request | Runs one nonstreaming chat or embedding request on the assigned process | Route receipt, placement, admission mode, prompt-free metrics, and Ollama response |

The natural-language path is intentionally two-stage. The small model cannot
name a final model. It returns only a task, objective, context hint, and tool or
vision requirements. Deterministic guards correct explicit requirements before
the ordinary router selects a model.

Calling `natural-routes` does not run the selected task. A client must use the
returned profile or submit a structured `tasks` request. There is no atomic
natural-language route-and-run endpoint in the current release.

Chat and embedding tasks forward route options. A captured-upstream contract
test verifies embedding option parity.

## What Ollama already optimizes

Across supported backends, Ollama provides the following behavior without FreeLlama:

- The runtime selects supported acceleration such as Metal, CUDA, ROCm, or Vulkan from the host and
  its process-level device visibility.
- The scheduler fits model, context, and compute allocations to available
  device memory.
- The runner selects CPU threads and GPU execution; applications do not need to
  divide one inference manually between the CPU and GPU.
- Ollama manages request queues plus model loading and eviction. It supports
  same-model parallel requests when memory permits.
- Ollama reports token counts plus load, prompt-processing, and generation
  durations in API responses.
- Ollama supports request-specific context length and model residency through
  `num_ctx` and `keep_alive`.

Ollama's documented concurrency controls are global server settings. Increasing
`OLLAMA_NUM_PARALLEL` multiplies context memory by the parallel-request count.
Increasing `OLLAMA_MAX_LOADED_MODELS` does not create memory; models still need
to fit. Leave both at their defaults until a concurrent workload benchmark
shows that a change improves completed tasks per hour without memory pressure.

## Current Mac audit

This audit captures the following snapshot from September 1, 2026. Treat it as machine
evidence, not a portable default.

| Item | Observed value | Interpretation |
|---|---|---|
| Hardware | Apple M4 Pro, 14 CPU cores, 48 GB unified memory | CPU and GPU share the memory system |
| macOS | 15.7.2 | Native Ollama supports this Apple-silicon system |
| Ollama server | 0.33.2 | Active application server on `127.0.0.1:11434`; re-read with `doctor` rather than trusting this row |
| First CLI on `PATH` | `/usr/local/bin/ollama`, 0.33.2 | Matches the active server |
| Metal device budget | 36 GiB reported by Ollama | This is the scheduler's observed device budget, not all 48 GB |
| Default context | 32,768 tokens | Ollama selected its 24–48 GiB device-memory tier |
| Parallel requests | 1 | Appropriate baseline for interactive and large-model tests |
| Keep-alive | 5 minutes | Ollama default |
| Loaded-model limit | `OLLAMA_MAX_LOADED_MODELS=2` | Explicit service configuration for the measured topology |
| K/V-cache type | `OLLAMA_KV_CACHE_TYPE=q8_0` | Explicit memory-saving configuration; requalify quality per important model |
| Flash Attention | `OLLAMA_FLASH_ATTENTION=1` | Explicitly enabled; required for quantized K/V cache |
| GGUF CPU threads | 10 worker threads | Ollama selected the 10 performance cores rather than all 14 cores |
| MLX | GPU runner observed | MLX models use their own runner path, not the GGUF tuning path |
| Power mode | Automatic on battery and AC power | High Power Mode remains an unmeasured experiment |

The server log also showed Metal fusion, concurrent Metal execution, graph
optimization, and automatic parameter fitting. FreeLlama must not claim these
as its own optimizations.

### Verify CLI and server alignment

The selected CLI is `/usr/local/bin/ollama`, and both the CLI and active server report 0.33.2.
Verify this alignment after changing Ollama installations or service definitions:

```bash
command -v ollama
ollama --version
curl --silent http://127.0.0.1:11434/api/version
```

The client and server versions must match before you test a CLI feature. A mismatch does not prove
an inference slowdown, but it can produce incompatible commands, flags, or diagnostics.

## What FreeLlama adds today

The active Rust implementation adds:

- capability filtering before speed ranking;
- explicit `fastest`, `balanced`, and `quality` evidence contracts;
- fail-closed quality routing when the policy has no qualified model;
- task-specific context, output, thinking, and tool-validation profiles;
- session affinity and preference for an eligible resident model;
- separate transition locks for primary and CPU backends, with shared execution permits for
  resident tasks and an exclusive permit for a cold transition;
- exact-model placement on an optional second CPU Ollama process, including `num_gpu=0` for its
  managed requests;
- guarded agent placement preferences plus a normalized, three-warm-sample, 10%-advantage feedback
  loop for `fastest` and `balanced` work;
- independent weighted GPU and CPU admission pools, so a GPU burst cannot starve a CPU helper;
- prompt-free load, prompt-processing, and output-generation metrics derived
  from Ollama's response fields;
- a local natural-language intent interpreter that cannot choose a model;
- an Ollama-compatible loopback endpoint and an escape hatch to port 11434.

These features can reduce reloads or avoid an unsuitable model. They do not
change the selected model's prompt-processing or decode rate.

The measured dual-backend workload improved median completion time from 37.997 to 28.233 seconds
(1.346 times, or 25.70%). Qwen reported 19,175,677,668 GPU-resident bytes, Nomic reported zero,
and all requests carried correct backend receipts with zero FreeLlama queue wait. One of three
parallel trials was slower than sequential, so this remains a workload-level optimization rather
than an inference-speed claim.

## Unsupported optimizations

FreeLlama does not provide:

- memory-pressure admission or a minimum-free-memory guard;
- transition coordination for native passthrough requests or processes that
  bypass the managed `tasks` endpoint;
- live thermal, power-mode, or swap-aware routing;
- online token-rate, error-rate, or cold-load-duration scoring;
- per-engine tuning for GGUF compared with MLX;
- dynamic `keep_alive` or concurrency limits by workload;
- an atomic natural-language route-and-run operation;
- held-out quality policies for completion, coding, tools, browser, vision, or
  long-context tasks in the example policy;
- quality-aware or thermal-aware CPU/GPU load balancing. Automatic placement uses persisted,
  prompt-free warm-latency aggregates only within exact operator-owned assignments.

The RFC describes several of these capabilities as planned phases. Do not treat
the RFC as evidence that they have shipped.

## Optimization decisions

Use this table before changing Ollama or FreeLlama:

| Candidate change | Default decision | Reason |
|---|---|---|
| Force layers of one model between CPU and GPU | Do not add | Ollama already fits and offloads the model; partial CPU fallback usually reduces decode throughput |
| Assign different models to isolated CPU and GPU-capable processes | Supported, benchmark per workload | Process isolation is reliable, but CPU speed and shared-memory pressure still determine whether overlap helps |
| Add macOS kernel or `sysctl` tuning | Do not add | Ollama does not document a supported, product-specific kernel setting. Unified-memory behavior belongs to macOS and Metal |
| Run Ollama in Docker on macOS | Do not use for performance | Docker Desktop does not provide Ollama GPU acceleration on macOS |
| Increase context globally | Do not add | Context memory grows with length; set the smallest sufficient `num_ctx` per request |
| Set parallel requests above 1 | Benchmark first | It can improve concurrent throughput but multiplies context memory and can hurt interactive latency |
| Pin multiple 18–21 GB models | Do not add by default | The observed 36 GiB Metal budget cannot safely establish that two heavy models plus contexts fit |
| Force Flash Attention | Benchmark per engine and model | The inspected GGUF path already enabled it automatically; MLX has a different runner |
| Use `q8_0` K/V cache | **8/10; qualify, then prefer for parallel/long-context work** | Ollama documents roughly half the KV memory of `f16` with very small precision loss; it is the practical companion to higher `OLLAMA_NUM_PARALLEL`, but it remains a process-wide quality tradeoff |
| Use `q4_0` K/V cache | Reject as a default | It saves more memory with a larger possible quality loss |
| Enable High Power Mode | Sustained-load experiment only | Apple documents higher sustained performance. Local token-rate and energy effects are not measured |
| Keep the small intent model resident | Keep bounded | It reduces routing latency but consumes memory and can affect a large-model transition |

## Measurement contract

Measure optimization with Ollama's response fields:

- prompt tokens per second = `prompt_eval_count / prompt_eval_duration` in
  seconds;
- output tokens per second = `eval_count / eval_duration` in seconds;
- cold-load cost = `load_duration`;
- end-to-end latency = `total_duration` plus measured FreeLlama overhead;
- workload throughput = successful, quality-qualified tasks per hour.

Compare one variable at a time against stock Ollama. Include cold and warm runs,
fixed prompts, fixed output budgets, exact-output guardrails, quality guardrails, resident
memory, swap, and failure counts. A higher token rate is not an improvement if
quality falls, requests fail, or model transitions dominate the workload.

Use `cargo run -- bench-all` (`packages/rust-core/src/model_bench.rs`) for model selection evidence.
Use [CPU and GPU model routing](CPU_GPU_ROUTING.md) for the separate-process benchmark and
verification procedure.

- Local-model router RFC — lived in the gitignored `.octocode/` workspace and is no longer in the tree; the shipped design it argued for is [ARCHITECTURE.md](ARCHITECTURE.md)

## Recommended next work

1. Keep the CLI executable aligned with the active Ollama server; `doctor` reports the selected
   path, both versions, and effective memory-related settings.
2. Back up the versioned prompt-free feedback snapshot with the policy and deployed binary.
   Verified warm work-unit latency and queue wait survive restart; raw prompts are never stored.
3. Implement memory admission only after a frozen alternating-model workload
   reproduces pressure losses. Keep Ollama as the final loading authority.
4. Extend transition coordination only if managed streaming or another parsed
   execution path needs it. Native passthrough remains Ollama-owned.
5. Evaluate request-specific context and residency profiles before testing
   global Flash Attention, K/V cache, or concurrency changes.
6. Promote a hardware-and-engine profile only after `benchmark/hardware/run_validation.py` and its
   held-out quality workload pass on a real self-hosted runner.

## Sources

- [Ollama FAQ and server settings](https://docs.ollama.com/faq)
- [Ollama context length](https://docs.ollama.com/context-length)
- [Ollama hardware and Metal support](https://docs.ollama.com/gpu)
- [Ollama API usage metrics](https://docs.ollama.com/api/usage)
- [Ollama embedding API](https://docs.ollama.com/api/embed)
- [Ollama macOS support](https://docs.ollama.com/macos)
- [Apple power modes](https://support.apple.com/en-lamr/101613)
