# Ollama-aware completion planning

Implementation plan, 1 November 2026. This follows the
[resource audit](OLLAMA_RESOURCE_AUDIT_2026-11-01.md#15-focused-adoption-plan-combine-what-freellama-already-knows).

## Ownership

Ollama continues to own model loading, automatic context selection when suitable, token scheduling,
prefix/KV caching, device offload and inference. FreeLlama owns eligibility, bounded admission,
shared host resource forecasts, lifecycle observations and explainable routing. The calling agent
owns prompts, dependencies and verification. No new cache engine, tensor migration layer or model
downloads are needed.

## Implementation sequence

1. Correct lifecycle decision inputs: compatible runner identity/context, forecast release versus
   observation generations, disjoint admission timing, confirmed unload receipts and no replay of
   an accepted MCP job.
2. Retain bounded exact-profile histories across models and restarts, with schema migration and
   idempotent persistence merging. Keep prompts out of profile identities and telemetry.
3. Share execution context preparation with routing, preserving Ollama defaults and existing
   resident context reuse. Filter payload-context failures before automatic ranking and enforce
   quality gates before backend partitioning.
4. Estimate setup + prefill + decode for qualified idle resident candidates using Ollama metrics.
   Change automatic placement only for a material predicted advantage, retaining operator pins,
   affinity, confidence and capability constraints. Unknown busy queues/cold loads use existing
   conservative policy, not fabricated latency. Return the candidate estimates in the receipt.
5. Make predictive warming fail fast at admission, transition and resources, without invoking
   eviction. A cancelled warm must release its prediction lease.
6. Validate deterministic behavior, formatting, Rust checks and MCP checks. Live paired workload
   measurements remain necessary before claiming a speedup or enabling broader queue reordering.

## Completion estimate contract

`execution.completion_planning` is an additive receipt, not a new request field or MCP tool.
It identifies the exact profile and reports estimated input/output work, setup/prefill/decode
nanoseconds, whether the estimate influenced placement, and reasons for unavailable estimates.
It is a snapshot and reserves nothing. Predictions are not measurements of saved time.

The first enabled comparison is deliberately limited to compatible, idle, observed resident
candidates with sufficient exact-profile samples. Historical average queue waits are diagnostic;
they do not predict a queue whose active request sizes are unknown. Prompt processing assumes
full prefill and takes no speculative cache credit. Actual Ollama cached-token counts distinguish
uncached prefill samples from cache hits. Embeddings use input work and no decode phase.

Explicit model/session affinity, explicit processor preference and the quality objective retain
their existing ownership. Balanced routing cannot obtain an unqualified CPU candidate by applying
the quality gate to an isolated backend after a qualified model exists elsewhere.

## Validation and remaining expansion

Regression coverage targets multi-model history retention, repeated snapshot merging, legacy
migration, payload fit, global quality gates, phase prediction reversals, unknown/overflow fallback,
cache-hit normalization, stale resource snapshots, confirmed unload outcomes, speculative warm
contention and accepted-job result loss.

Broader completion planning for cold/busy candidates requires calibrated load/queue estimates and
contention measurements. Bounded model-locality queue bursts and shared retention/preload economics
remain follow-on policies; they should use these receipts and measured workload traces rather than
change fairness on untested predictions. The audit describes the paired benchmark matrix.

The existing idle-eviction activity check is still a snapshot. Closing its race with new managed
or raw arrivals requires a shared per-model lifecycle lease; confirmed unload receipts alone do
not close that race. This change does not claim to repair every finding in the research audit.

The implementation reuses Ollama's request-control projection for profile comparisons without
copying prompts or images. Thread counts default to Ollama's own detection on all platforms;
the dedicated CPU backend still supports an explicit `FREELLAMA_CPU_NUM_THREAD` override.

No model pulls, live unloads or server configuration changes are part of this implementation.

## Verification recorded

Local Octocode AST searches and LSP reference queries verified the existing routing/feedback
paths and the new planner's selection and observation call sites. Six implementation/review
tracks shared file ownership; existing repository edits were preserved.

| Check | Result |
|---|---|
| `yarn typecheck` (final) | Passed |
| `cargo fmt --all --check` | Passed |
| `RUSTC_WRAPPER= cargo clippy --offline --all-targets -- -D warnings` | Passed |
| Focused MCP job/progress/receipt tests | 49 passed |
| `yarn test:js` | 853 passed; 12 failed fixture tests accompanied by 11 loopback `listen EPERM` errors |
| `RUSTC_WRAPPER= yarn test:rust` | CLI unit tests passed; workspace execution stopped at three CLI fixture listener permission failures |
| `RUSTC_WRAPPER= cargo test --offline -p freellama-core --lib` (final) | 257 passed, 7 failed, 1 ignored; six mock listener permission failures and one denied host-memory discovery assertion |
| Whitespace check for changed implementation/documentation files | Passed |

The inherited compiler wrapper was bypassed because `sccache` was denied by this environment.
Final test/typecheck processes experienced startup delays but completed. The full suite is not
green here; mock lifecycle contracts compile but their socket-dependent behavior still requires
an environment that permits loopback listeners. No new physical CPU/GPU performance result is
claimed. The paired workload matrix in the audit remains the acceptance gate for speedup claims.
