# Context and execution review — 2026-09-06

The implementation follows the request path from adapter budget fitting, through managed routing,
admission and model transitions, to Ollama, placement observation, and returned statistics.

## Verified upstream contracts

- Ollama **v0.33.2** already accepts `truncate:false` and `shift:false` in
  [ChatRequest](https://github.com/ollama/ollama/blob/v0.33.2/api/types.go#L147).
  [The server](https://github.com/ollama/ollama/blob/v0.33.2/server/routes.go#L2519)
  applies the truncation setting and disables truncation for MLX. The earlier blanket statement
  that Ollama always silently truncates was too broad; behavior depends on backend and request.
- [Ollama v0.33.3 metrics](https://github.com/ollama/ollama/blob/b79067b0db7417f20108363bc22adb97f35c966a/api/types.go#L557)
  distinguish missing cache counts from zero. Total prompt count includes cache hits, while
  prompt duration measures uncached work. Throughput therefore uses total minus cached tokens.
- [Ollama's KV calculation](https://github.com/ollama/ollama/blob/v0.33.3/fs/ggml/ggml.go#L614)
  uses architecture-specific dimensions, cache precision, and parallelism. A generic F16 estimate
  is not a lower bound because actual cache precision can be quantized.
- [llama.cpp prefix reuse](https://github.com/ggml-org/llama.cpp/blob/74a7c897f049c17e7080423aa2111776eff6ebbf/tools/server/server-context.cpp#L3201)
  starts with the exact common prefix. Keeping fitting conversation prefixes stable preserves
  reuse opportunities; rewriting old observations can invalidate them.
- [Anthropic context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
  supports retaining important facts and retrievable references during compaction. It does not
  establish one universal compaction threshold for every local model.

## Implementation and validation

1. Hard-bound pages, including single huge lines; stop paging at the final page. Preserve exact
   reassembly and stored-page recovery actions during deterministic compaction.
2. Compact eligible old observations toward 90% of budget after overflow. Preserve fitting
   prefixes, pinned messages, and existing breadcrumbs. Separate persisted calibration by endpoint,
   verified immutable model digest, system-prompt hash, and estimator configuration. Unknown
   identity disables persistence, not process-local calibration.
3. Size managed text contexts from prompt/schema estimates plus output/template reserves. Preserve
   explicit context and multimodal profiles. Send upstream overflow controls for chat and embeddings.
4. Scope context/KV metadata to the declared architecture and return unknown for unsupported
   cache layouts. Report F16 assumptions and local-memory applicability explicitly.
5. Share primary-backend exclusion between raw mutations and managed work. Hold guards through
   the response body lifetime; retain metadata access. Do not replay timeout/disconnect failures.
6. Report queue and transition waits separately. Correct managed and benchmark prefill rates and
   adapter cache totals, preserving missing/partial reporting.
7. Hold new local work on measured host pressure, with hysteresis and bounded waits. Reserve
   forecast memory across CPU/GPU pools; revalidate after transition locking. Learn bounded runner
   footprints only from verified matching digest/context observations. Unknown signals remain unknown.
8. Prevent large-request starvation with capacity reservation after six bypasses. Share one deadline
   across admission, resources, and transition locking. Release permits on cancellation.
9. Spool observations and page offsets to temporary disk; stream full audit results atomically.
   Preserve action/status/source/page/provenance ledgers during compaction. Default managed CPU
   thread requests to half the logical cores while preserving explicit overrides.

The orchestration work split resource sampling, context storage, and fairness/integration into
independent agent-owned changes, followed by shared-flow review and deterministic contract tests.
See [execution architecture](ARCHITECTURE.md#managed-task-execution) and
[CLI policy controls](CLI.md#bound-admission) for the implementation contract.

Regression checks cover pagination reassembly, prefix stability, estimator identity, adapter call
counts, cache-count semantics, architecture collisions, raw/managed exclusion, stream cancellation,
HTTP 504 refusal, output-reserve overrides, and exact request/receipt agreement.

A bounded live smoke on Ollama 0.33.2, Apple M2 Pro, 32 GiB unified memory used installed
`gemma4:latest`. It returned exactly `FREELLAMA_OK`, selected 2,048 context tokens, reported 17 prompt
and 6 output tokens, verified GPU placement, and verified immediate unload. Cache counts were
unreported and remained null. This is execution evidence, not a sustained-load or quality benchmark.

The follow-up governor smoke used a temporary gateway with a two-second queue deadline. It refused
the installed 18.17 GB `qwen3.8:27b-mlx` cold load with HTTP 503 in 2.10 seconds while estimated
available RAM was 11.11 GB and the reserve was 5.15 GB. Ollama remained empty. A bounded
`glm-ocr:latest` request then completed with HTTP 200 in 1.99 seconds, 2K context, and six requested
threads; placement and immediate unload were verified. The next request used the observed 1.85 GB
same-digest footprint instead of the initial 2.35 GB file-plus-KV estimate. Both calls unloaded;
final health reported zero reservations and normal OS memory pressure. Thermal data was unavailable.
This OCR-model smoke validates admission and lifecycle behavior, not text-answer suitability.

Initial checks: 207 Rust tests passed; the separate ignored-by-default live telemetry check also passed.
The adapter suite passed 20 unittest cases and 94 context contracts. JavaScript unit tests, TypeScript
checks, 29 MCP integration tests (four skipped), strict Clippy, and formatting/diff checks passed.
The full live E2E suite was not run because it includes a model pull/delete lifecycle test.
CPU routing and
cross-backend memory races were tested with controlled backends; no separate live CPU Ollama daemon
was configured or benchmarked. Existing model files and global Ollama settings were unchanged.

## Follow-up review fixes and recheck

The follow-up review identified five concrete gaps, now addressed:

1. Missing RAM telemetry previously admitted work by default. Local admission now defaults to
   `require_memory`; `require_all` requires the collector's applicable signals, and `best_effort`
   is an explicit opt-in. Both `serve` and standalone `proxy` expose the policy flag.
2. Preview readiness previously considered queue slots without the selected model's memory cost.
   Preview and admission now share the same pure capacity assessment and footprint lookup.
   Previews subtract existing reservations without acquiring permits themselves.
3. Resource errors previously embedded JSON in an error string. Managed, natural-route, and batch
   failures now retain readable errors alongside typed codes and structured admission receipts.
4. The platform entry module fell from 3,513 to 1,990 lines by extracting admission, execution,
   readiness, and error modules with narrow internal interfaces. This separates responsibilities;
   it does not claim that the remaining execution module needs no further decomposition.
5. A [bounded mixed-workload probe](../benchmark/local/docs/mixed-workload.md) now freezes model,
   fixture, harness, and workload identities; penalizes refusals and unfinished work; records raw
   responses and telemetry; and stops dispatch on unsafe or uncertain completion conditions.

Live validation found an additional bug: Ollama returned `done:false` for a non-streaming chat,
and the old managed path treated it as successful execution. The fixed path returns HTTP 502 with
`code: upstream_incomplete_response`, preserves the upstream response, excludes it from feedback
and session binding, and honors the request's immediate unload. Controlled tests also cover
HTTP-200 error objects and unsuccessful upstream statuses, with verified placement available so
missing feedback cannot be attributed merely to absent placement evidence.

The preliminary 4K baseline is preserved: the full prompt contained 4,783 actual tokens and Ollama
refused it rather than truncating. A new experiment used the same frozen harness on both builds
with 8K context, the installed `glm-ocr:latest` digest, three planned trials of five tasks,
96 output tokens, two concurrent requests, and a two-second gateway queue deadline.

| Observation | Baseline | Fixed build |
|---|---:|---:|
| Correct / planned tasks | 0 / 15 | 2 / 15 |
| Incorrect responses | 5 | 2 |
| Queue refusals | 10 | 4 |
| Explicit incomplete upstream responses | 0 | 1 |
| Not dispatched after safety stop | 0 | 6 |
| Minimum sampled available RAM | 5.80 GB | 7.48 GB |
| New swap-out pages | 0 | 0 |

The fixed run stopped after its typed incomplete-response failure and verified the requested
unload. The runner's generic HTTP-failure bucket labels that response `transport_error`; the raw
receipt identifies `upstream_incomplete_response`, not a lost network connection. The baseline
took 23.43 seconds; the partial candidate took 13.44 seconds. These are not comparable completion
times. The comparator returned **INSUFFICIENT_EVIDENCE**: thermal telemetry was unknown and the
candidate was incomplete. This OCR model and short queue deadline do not establish general
text quality or optimal scheduling. No quality, compaction, or throughput improvement is claimed.
Raw reports are retained locally under `.octocode/octocode-eval-benchmark/` as
`mixed-baseline.json`, `mixed-baseline-8k.json`, `mixed-candidate-8k.json`, and
`mixed-comparison-8k.json`; these generated reports are not tracked source artifacts.

A final Qwen preview reported an available queue slot but `held_insufficient_capacity` and zero
admissible tasks. Execution refused its estimated 18.17 GB cold load with structured HTTP 503 in
2.18 seconds, against 11.36 GB available and a 5.15 GB reserve. No model loaded, and reservations
returned to zero. Live telemetry sampling also passed; thermal observation remained unknown.

Recheck: 227 Rust tests, 41 Python unittest cases plus 94 context contracts, 62 JavaScript tests,
and 29 MCP integration tests passed (four MCP tests skipped). Formatting, strict Clippy, TypeScript,
release/native builds, and diff checks passed. An independent reviewer approved the bounded fixes
after checking shared admission/readiness behavior, structured errors, benchmark guards, and
completion-before-feedback ordering. The same live CPU and model-lifecycle E2E exclusions above
still apply; direct Ollama callers and global resource enforcement remain outside this layer.

## Measurement boundaries

The primary MLX model lacks enough metadata for a KV estimate. Available RAM on macOS is a bounded
reclaimable-memory estimate, not an exact allocation prediction. Linux uses
[MemAvailable](https://docs.kernel.org/filesystems/proc.html); container/cgroup limits are not sampled.
Remote runner resources, discrete GPU free VRAM, actual KV precision, and parallelism are not inferred
from gateway settings. A missing thermal observation stays null. Raw proxy requests receive pressure
gating, but not the managed route's model-footprint forecast. The governor cannot preempt inference
or constrain other processes; direct Ollama callers bypass FreeLlama. Ollama owns final runner admission.
Its [concurrency and KV guidance](https://docs.ollama.com/faq) remains relevant to operator tuning.

Token estimates remain approximate; upstream overflow refusal is a second boundary on supported
backends. A subprocess can still transiently produce its full output in memory before it is spooled;
accumulated observations and final audit serialization no longer retain those full strings. Model
identity is verified at calibration setup, not atomically locked against an external mid-run tag
replacement. Semantic summaries and KV quantization require workload-quality evaluation before a
default change. No sustained-load, semantic-quality, or maximum-throughput claim follows from smoke tests.
