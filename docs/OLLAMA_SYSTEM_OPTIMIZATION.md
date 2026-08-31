# Ollama and FreeLlama optimization boundary

Ollama already owns inference optimization. FreeLlama can improve completed
work per hour by choosing an eligible model and avoiding unnecessary model
changes, but it does not make an individual model decode faster.

This page explains the active request flow, the settings Ollama owns, the
current machine audit, and the optimization work that remains.

## Decision

Use stock Ollama as the inference runtime. Keep FreeLlama as an optional Rust
control plane for evidence-qualified routing, bounded request profiles, session
affinity, and policy enforcement.

Do not advertise FreeLlama as an accelerator or an Ollama inference plugin. The
current implementation does not tune kernels, split work between the CPU and
GPU, manage macOS memory pressure, or improve tokens per second.

## Request flow

FreeLlama has four distinct paths:

| Request | FreeLlama work | Ollama work | Result |
|---|---|---|---|
| Native `/api/*` or `/v1/*` | Streams bytes through the compatibility proxy | Performs normal inference and scheduling | Exact Ollama response |
| `POST /_freellama/v1/routes` | Discovers models, filters capabilities, and applies policy | Supplies installed-model and residency data | Model and request profile; no inference |
| `POST /_freellama/v1/natural-routes` | Normalizes a schema-bound intent, then runs the deterministic router | Runs the small intent model | Model and request profile; selected task does not run |
| `POST /_freellama/v1/tasks` | Selects a route, applies managed-task admission, and constructs one bounded request | Runs one non-streaming chat or embedding request | Route receipt, admission mode, prompt-free metrics, and Ollama response |

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

On Apple silicon, Ollama provides the following behavior without FreeLlama:

- The native macOS binary includes Metal GPU acceleration.
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

This audit captures the following snapshot from August 23, 2026. Treat it as machine
evidence, not a portable default.

| Item | Observed value | Interpretation |
|---|---|---|
| Hardware | Apple M4 Pro, 14 CPU cores, 48 GB unified memory | CPU and GPU share the memory system |
| macOS | 15.7.2 | Native Ollama supports this Apple-silicon system |
| Ollama server | 0.33.2 (was 0.32.15 when first measured) | Active application server on `127.0.0.1:11434`. Re-read with `doctor` rather than trusting this row |
| First CLI on `PATH` | Homebrew 0.13.5 | Version mismatch; fix before relying on CLI-specific behavior |
| Metal device budget | 36 GiB reported by Ollama | This is the scheduler's observed device budget, not all 48 GB |
| Default context | 32,768 tokens | Ollama selected its 24–48 GiB device-memory tier |
| Parallel requests | 1 | Appropriate baseline for interactive and large-model tests |
| Keep-alive | 5 minutes | Ollama default |
| Loaded-model limit | Automatic | Ollama decides from available memory |
| Flash Attention | Automatic and enabled for the inspected GGUF runner | Forcing the environment flag is not proven to improve this path |
| GGUF CPU threads | 10 worker threads | Ollama selected the 10 performance cores rather than all 14 cores |
| MLX | GPU runner observed | MLX models use their own runner path, not the GGUF tuning path |
| Power mode | Automatic on battery and AC power | High Power Mode remains an unmeasured experiment |

The server log also showed Metal fusion, concurrent Metal execution, graph
optimization, and automatic parameter fitting. FreeLlama must not claim these
as its own optimizations.

### Resolve the CLI mismatch

The Ollama application CLI is available at `/usr/local/bin/ollama`, while the
first CLI on this machine is the older Homebrew installation at
`/opt/homebrew/bin/ollama`.

For one shell session, put the application CLI first on `PATH`. Then verify both
versions:

```bash
export PATH="/usr/local/bin:$PATH"
hash -r
ollama --version
curl --silent http://127.0.0.1:11434/api/version
```

The client and server versions must match before testing a CLI feature. This
mismatch does not prove an inference slowdown, but it can produce incompatible
commands, flags, or diagnostics.

## What FreeLlama adds today

The active Rust implementation adds:

- capability filtering before speed ranking;
- explicit `fastest`, `balanced`, and `quality` evidence contracts;
- fail-closed quality routing when the policy has no qualified model;
- task-specific context, output, thinking, and tool-validation profiles;
- session affinity and preference for an eligible resident model;
- shared execution permits for resident managed tasks and an exclusive permit
  for nonresident managed-task transitions;
- prompt-free load, prompt-processing, and output-generation metrics derived
  from Ollama's response fields;
- a local natural-language intent interpreter that cannot choose a model;
- an Ollama-compatible loopback endpoint and an escape hatch to port 11434.

These features can reduce reloads or avoid an unsuitable model. They do not
change the selected model's prompt-processing or decode rate.

## What FreeLlama does not add yet

The current implementation does not provide:

- memory-pressure admission or a minimum-free-memory guard;
- transition coordination for native passthrough requests or processes that
  bypass the managed `tasks` endpoint;
- live thermal, power-mode, or swap-aware routing;
- online token-rate, error-rate, or load-duration scoring;
- per-engine tuning for GGUF compared with MLX;
- dynamic `keep_alive` or concurrency limits by workload;
- an atomic natural-language route-and-run operation;
- held-out quality policies for completion, coding, tools, browser, vision, or
  long-context tasks in the example policy.

The RFC describes several of these capabilities as planned phases. Do not treat
the RFC as evidence that they have shipped.

## Optimization decisions

Use this table before changing Ollama or FreeLlama:

| Candidate change | Default decision | Reason |
|---|---|---|
| Force CPU and GPU layers manually | Do not add | Ollama already fits and offloads the model; partial CPU fallback usually reduces decode throughput |
| Add macOS kernel or `sysctl` tuning | Do not add | Ollama does not document a supported, product-specific kernel setting. Unified-memory behavior belongs to macOS and Metal |
| Run Ollama in Docker on macOS | Do not use for performance | Docker Desktop does not provide Ollama GPU acceleration on macOS |
| Increase context globally | Do not add | Context memory grows with length; set the smallest sufficient `num_ctx` per request |
| Set parallel requests above 1 | Benchmark first | It can improve concurrent throughput but multiplies context memory and can hurt interactive latency |
| Pin multiple 18–21 GB models | Do not add by default | The observed 36 GiB Metal budget cannot safely establish that two heavy models plus contexts fit |
| Force Flash Attention | Benchmark per engine and model | The inspected GGUF path already enabled it automatically; MLX has a different runner |
| Use `q8_0` K/V cache | Qualification experiment | It can reduce GGUF context memory. This global setting can change quality, especially for high-GQA models |
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

Use `cargo run -- bench-all` (`packages/rust-core/src/model_bench.rs`) for model selection evidence:

- Local-model router RFC — lived in the gitignored `.octocode/` workspace and is no longer in the tree; the shipped design it argued for is [ARCHITECTURE.md](ARCHITECTURE.md)

## Recommended next work

1. Extend `doctor` with the resolved CLI path and effective Ollama settings that
   are observable without reading prompts. It already reports CLI and server
   versions and whether they match.
2. Persist prompt rate, decode rate, load time, resident bytes, selected engine,
   and failures in bounded, prompt-free receipts. Task responses expose rates
   but don't store them.
3. Implement memory admission only after a frozen alternating-model workload
   reproduces pressure losses. Keep Ollama as the final loading authority.
4. Extend transition coordination only if managed streaming or another parsed
   execution path needs it. Native passthrough remains Ollama-owned.
5. Evaluate request-specific context and residency profiles before testing
   global Flash Attention, K/V cache, or concurrency changes.
6. Add a hardware-and-engine profile only when its held-out workload beats the
   stock configuration and preserves quality.

## Sources

- [Ollama FAQ and server settings](https://docs.ollama.com/faq)
- [Ollama context length](https://docs.ollama.com/context-length)
- [Ollama hardware and Metal support](https://docs.ollama.com/gpu)
- [Ollama API usage metrics](https://docs.ollama.com/api/usage)
- [Ollama embedding API](https://docs.ollama.com/api/embed)
- [Ollama macOS support](https://docs.ollama.com/macos)
- [Apple power modes](https://support.apple.com/en-lamr/101613)
