# Ollama coordination hardening

Implementation follow-up, 1 November 2026, to the [resource audit](OLLAMA_RESOURCE_AUDIT_2026-11-01.md) and [before/after comparison](OLLAMA_BEFORE_AFTER_COMPETITORS_2026-11-01.md). The earlier reports describe their inspected snapshots; this document records the subsequent fixes.

Later review: [implementation recheck, additional repairs and current ratings](OLLAMA_RECHECK_2026-11-01.md). Its verification results are separate from the runs recorded here.

The implementation keeps inference, model allocation, device offload and KV management in Ollama. FreeLlama coordinates eligibility, bounded work, memory forecasts and lifecycle ownership. Rust manages these control structures; it does not implement model paging or make the engine's kernels faster.

## Findings and changes

| Finding | Implemented repair |
|---|---|
| F01: stale host samples after forecast release | Reservation release invalidates the observation generation; cached and previously returned pre-release samples cannot approve new capacity |
| F02: name-only runner reuse | Reuse requires compatible digest, effective context and observed placement; unobservable explicit load controls remain exclusive |
| F03: idle-check/unload races | Per-model RAII lifecycle claims coordinate managed work, raw work and eviction; request and confirmation ownership are explicit |
| F04: unaccounted raw cold work | Local cold inference needs a positive conservative forecast; unavailable sizing is a typed refusal. Compatible observed runners can downgrade transition ownership; dropped/incomplete streams do not count as successful execution |
| F05: speculative warm can displace work | Strict idle admission and memory checks refuse speculation rather than queueing or evicting |
| F06: mutable tags inherit old qualification | Quality policies and benchmark metadata bind evidence to a digest; evidenced automatic execution checks identity at dispatch and after execution |
| F07: accepted jobs can replay after result loss | Accepted identities remain authoritative; unavailable results do not authorize another inference |
| F08: mixed embedding spaces and incomplete receipts | Slices pin tag, digest, endpoint and backend; verify effective controls, dimensions, cardinality and finite values; retain per-slice evidence and aggregate timing |
| F09: failed research commands become evidence | Structured subprocess status distinguishes success, legitimate no-match and error; failed/empty outputs do not become grounding evidence |
| F10: payload fit checked too late | Automatic candidates are filtered using payload context before final selection |
| F11: alternating models erase feedback | Bounded exact-profile histories, migration and idempotent merges retain independent model observations |
| F12: overlapping wait timers | Queue/resource timing is disjoint; unload timing and outcomes describe actual observations |
| F13: arbitrary timer implies cold disk | Load learning requires observed runner transitions; upstream setup duration remains distinct from disk-cold loading |
| F14: full metadata refresh and coupled failures | Bounded endpoint/digest metadata cache, independent expiry, targeted pinned discovery and partial healthy catalogs |
| F15: progress controls cancellation | MCP task/research cancellation uses addressable jobs independently of notifications; sliced/batched work preserves accepted IDs and deadlines |
| F16: diagnostic subprocess blocks async workers | Memory-holder diagnostics return fresh cached data immediately and refresh on one bounded background worker, outside the cache mutex |

F01, F02, F05, F07 and F10–F13 were substantially addressed in the preceding implementation; this pass closes additional identity, lifecycle and raw-traffic paths. Regression tests cover the repaired contracts. Socket-dependent tests still require an environment permitting loopback listeners.

## Memory and cache policy

The catalog separates installed tags, static metadata and live residency. Tags have a 30-second snapshot lifetime. `/show` entries use endpoint and nonempty model digest, with a five-minute absolute lifetime because engine-derived capabilities and defaults can change without a weight update. Cache access changes LRU order but does not renew freshness. `/api/ps` remains a live observation, and failed observation is unknown rather than an invented empty model list.

The metadata cache permits 128 entries, 4 MiB of serialized payload/key budget and 64 KiB per entry. Large tensor lists, tokenizer vocabularies, templates and licenses are not retained by this routing cache. A shared semaphore limits active metadata fetches to four per platform instance across both endpoints and independent pinned requests; each response body is bounded. Each catalog pass also limits spawned lookup tasks to four at a time. These are logical cache limits, not a claim that allocator overhead or process RSS equals the serialized byte count.

Cached values use `Arc` where shared ownership avoids copying the metadata tree. Short synchronous mutex sections only inspect or update bounded state. Network requests and subprocess sampling run outside those mutexes. Lifecycle guards release bookkeeping on cancellation; a released local permit is not proof that Ollama physically stopped an already accepted request.

Raw and managed workloads share resource forecasts. Host-wide free VRAM is retained as monitoring data but cannot discount a particular Ollama backend's host-memory forecast without verified device eligibility. This is deliberately conservative on discrete-GPU machines; exact per-device allocation and reservations remain an integration task requiring hardware evidence. CPU/GPU endpoints on unified-memory machines still compete for the same RAM.

Model activity retains bounded idle history while preserving live leases. Completion learning remains bounded by exact profile and sample count. No response cache, cross-model KV sharing, tensor spill engine or alternate allocator was added without a demonstrated need.

Raw `load_duration` remains reported telemetry. A failed reuse check can mean unknown identity or incompatible controls, so it does not teach reload cost. Legacy raw transition audit rows are also excluded when restoring learning after restart. Explicit unload supervisors retain their claims through bounded request and confirmation work; cancellation before dispatch can still skip cleanup, and a timeout never proves physical completion.

## Model identity and control compatibility

Task policies now include the artifact measured by the evaluation:

```toml
[policies.coding]
qualified_models = ["example:exact-tag"]
model_digests = { "example:exact-tag" = "sha256:actual-measured-digest" }
```

The digest must come from the evaluation record. Do not insert today's digest into historical evidence merely to restore a high-confidence route. Legacy policy/benchmark files remain readable, but missing or mismatched artifact identity does not qualify the current model. Explicit operator model pins and configured defaults remain separate from measured quality claims.

Strict continuation requests can carry `expected_model_digest`, `expected_execution_backend` (`cpu` or `primary`) and `expected_upstream`, together with an explicit model. Embedding slices use that contract automatically. The receipt includes `embedding_identity` with observation provenance and an explicit non-atomic scope; this is matching catalog/runner evidence rather than a response attestation. Dimensions alone do not identify an embedding space. A persistent retrieval index must preserve compatible model identity and controls for later query embeddings as well.

Ollama accepts inference by model tag. FreeLlama checks identity before dispatch and uses observed runner identity afterward; it cannot make an external tag replacement atomic with engine execution. Unknown or changed artifacts fail strict work rather than returning vectors with a false compatibility claim. Explicit trials without strict identity can execute, but stale qualification is downgraded and unverified behavior is excluded from model-fact learning.

Payloads derive their real capability requirements before routing: tools, thinking, images and audio, including multimodal embedding objects. Thinking effort names use the selected model's `/show` metadata. WAV content in Ollama's decision-image transport is classified as audio. The engine still owns actual payload decoding and backend-specific support. MCP audio preflight rejects freshly verified incompatible models; unavailable metadata leaves final validation to Ollama.

Nearest-neighbor ranking validates vector geometry and computes cosine without overflowing large finite components. Batched cancellation preserves input result order, bounded concurrency and the existing 3:2:1 priority dispatch weights. Progress reporting is optional presentation, not cancellation authority.

Pull notifications also use a monotonic counter across layer changes and final status. Per-layer byte counts and percentages stay in the message, so changing layers cannot reset the MCP progress value or imply a known total download size.

## Deferred execution and recovery

Deferred tasks retain their accepted identity through polling, cancellation and result errors. Automatic replay is prohibited after acceptance or an uncertain outcome. Cancellation receipts distinguish a request to cancel from acknowledged local completion.

Optional persistence uses the existing SQLite database through a dedicated FULL-sync connection and bounded blocking workers. It records receipts/results and hashed request identity, not executable prompts for later replay. Restarted workers are not resumed automatically. See [deferred-job recovery](DEFERRED_JOBS.md) for activation, retention, idempotency, payload eviction and unknown outcomes.

## Rust practices used

- Ownership and RAII encode resource and lifecycle release; tests exercise cancellation and competing arrivals.
- Standard mutexes protect short in-memory operations; asynchronous leases represent intentionally awaited ownership. No diagnostic process runs under a shared cache mutex.
- Blocking database work has explicit concurrency limits, with permits retained by the blocking closure if its async caller disappears.
- Cache size, result retention, queue depth, response bodies and sampling history have explicit limits.
- Checked arithmetic and explicit unknown states prevent unavailable telemetry from becoming zero cost or positive capacity.
- Reuse `Arc` for shared immutable data and consume JSON when reducing metadata; avoid unnecessary deep copies and speculative synchronization/allocator replacements.

Primary references: [Tokio shared state](https://tokio.rs/tokio/tutorial/shared-state), [Tokio cancellation](https://tokio.rs/tokio/tutorial/select), [bounded channels and backpressure](https://tokio.rs/tokio/tutorial/channels), [blocking work](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html), [Rust Arc](https://doc.rust-lang.org/std/sync/struct.Arc.html), and [SQLite WAL durability](https://www.sqlite.org/wal.html).

## Updated engineering ratings

These extend the earlier comparison using its original pre-audit baseline and five-point rubric: 1 missing/fragile, 2 partial, 3 useful with significant gaps, 4 strong foundation, 5 broadly validated and operationally mature. They assess code contracts, not measured speed or superiority over competitors. The mock-HTTP suites now pass; live-hardware validation is still missing, so no area is rated 5.

| Area | Initial / 5 | Current / 5 | Reason and remaining limit |
|---|---:|---:|---|
| Rust ownership and central policy | 4 | 4 | Explicit lifecycle ownership and bounded blocking work; engine execution remains external |
| Managed queue and fairness | 4 | 4 | Per-backend load lock separate from per-model use leases, so a cold load no longer waits behind unrelated decodes; a drain wait re-plans on entry; broader cold/busy scheduling remains uncalibrated |
| Memory admission correctness | 3 | 4 | Shared raw forecasts, stale-observation protection, raw generations without a resolvable model refused (`model_forecast_unavailable`), image payloads checked against declared minimum vision tokens; per-device VRAM reservation and real pressure tests remain |
| Idle retention and eviction | 3.5 | 4 | A model in use is never evicted; bounded supervised unload confirmation; failed unloads are not re-verified later |
| CPU/GPU routing efficiency | 3 | 3.5 | Compatible resident predictions, digest-qualified evidence and an observed-placement rule for CPU and GPU; predictive warm is refused on discrete GPUs; no hardware speed measurement yet |
| Many-model learning and metadata scale | 2.5 | 4 | Bounded profile histories; a tag changed during discovery is a typed 409 for pinned work; oversized metadata cached as a routing projection; expired entries swept; large-library load testing remains |
| Job/result/cancellation semantics | 2.5 | 4 | No accepted-job replay, bounded cancel retry with a reported cancellation state, HMAC-keyed stored request hashes; durable receipts are opt-in |
| Research evidence correctness | 2.5 | 3.5 | Failed tools cannot silently become grounding (tested, including mixed pipelines); model answers still require caller verification |
| Test and release health | n/a | 4 | All mock-HTTP and unit suites pass (see below); live checks pending |
| Proven speed advantage | Unrated | Unrated | Requires paired real-hardware workloads |

The [competitor review](OLLAMA_BEFORE_AFTER_COMPETITORS_2026-11-01.md) remains the source for product comparisons and articles. The implementation strengthens FreeLlama's resource coordination and evidence contracts above Ollama; it does not establish exclusive ownership of queueing, caching or model eviction as product ideas.

## Validation and limits

Checks on 2026-11-01, Mac14,10 with 32 GB, on a machine that permits loopback listeners:

| Check | Result |
|---|---|
| `yarn typecheck` | Passed |
| `cargo fmt --all --check` | Passed |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Passed |
| `yarn test:js` | 35 files, 936 passed |
| `cargo test --workspace --no-fail-fast` | CLI 14 + 5 passed; core unit 341 passed, 1 ignored; core integration 271 passed, 3 ignored |

The first run with sockets available found 11 deterministic integration failures that the sandboxed run had hidden. One was a product bug: a tag changed or `/api/tags` failed during discovery's confirmation read, and a pinned request got a generic 502. Pinned work now gets 409 `model_identity_changed` for a change and is served uncached during a confirmation outage. The lease-split work found a second one: a load waiting for memory missed the wake when the busy model went idle and waited until its deadline. The remaining failures were fixtures that lacked the digest and `context_length` real Ollama reports.

The live per-model run against the new build found two more:

- **Sliced embeddings never worked live.** The MCP slice check read the model name from the top of the task receipt, but the server sends it under `route`. Every embedding of more than one slice was refused as "identity is uncertain". The unit fixture carried the name in both places, which hid the bug. The check now reads `route.selected_model`, and the fixture matches the server.
- **An explicit `context_tokens` skipped the fit check.** The model loaded, and Ollama's 400 came back as `upstream_error`. Text that certainly overflows the window, counted at 8 bytes per token so a prompt that fits is never refused, is now refused before loading with `context_window_exceeded`. The automatic-cap refusal uses the same code.

No live models were downloaded, run, unloaded or deleted by this implementation task. Hardware throughput, p95 latency, cold-load behavior, power efficiency and GPU placement were not newly measured. The paired workload matrix in the audit remains necessary before claiming a speedup.

Broad cold/busy completion prediction, queue-locality reordering and calibrated retention economics remain future measured policies. The current planner uses qualified compatible idle residents and preserves conservative fallbacks. One coordinating FreeLlama authority per topology remains the deployment boundary; persistent receipts do not create distributed admission leases.
