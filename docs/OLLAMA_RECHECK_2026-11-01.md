# Ollama implementation recheck and ratings

Second source review on 1 November 2026, following the [hardening report](OLLAMA_HARDENING_2026-11-01.md). This review uses the current working tree, which includes substantial subsequent work from other sessions. Existing staged changes were preserved. It is not a comparison of published releases.

The core architecture remains appropriate: Ollama owns model execution and allocation; Rust owns bounded admission, resource observations, lifecycle coordination and routing. FreeLlama should be described as model residency management, not operating-system virtual memory or transparent tensor paging.

## What the repeat review found

Passing ordinary tests did not establish that every failure path was correct. The repeat review identified these specific gaps:

| Finding | Repair and acceptance check |
|---|---|
| Unconfirmed `/show` metadata avoided the digest cache but still entered the 30-second combined catalog | Propagate confirmation/cacheability through catalog assembly; skip the combined snapshot for unconfirmed results. Regression changes a tag during lookup, fails confirmation, and requires immediate fresh discovery |
| Correct-looking chat text with `done:false` or an embedded error could pass a model benchmark | Validate completed assistant response before grading; preserve the embedding response contract. Pure regressions cover false/missing/nonboolean completion and malformed/error responses |
| Research retried HTTP 503 even when the receipt explicitly prohibited replay | Check execution outcome and retry safety before retrying; unknown or accepted work cannot become another inference merely because its transport status is 503 |
| Successful `echo`/`printf`/diagnostic commands could count as research evidence | Preserve successful diagnostics in the audit, while requiring source-bearing lookup output for citations and grounding |
| Research grades trusted arbitrary JSON and mutable model names | Validate grade records and bind them to the measured artifact digest and consistent observed execution. Missing identity remains unqualified; no historical record receives an invented digest |
| Raw nonstream generation kept the load lock until response headers arrived | Poll runner readiness while forwarding headers, then carry any unfinished watcher into the response body |
| A same-model follower could wait on a full cold-load forecast after the runner became ready | Notify resource waiters on readiness and recompute the residency-aware footprint. The regression decreases simulated available memory when loading and keeps the first decode open |
| Raw room-making could skip a large context because the file-only estimate fit | Evaluate the effective context and full allocation requirement before deciding whether idle eviction is needed |

The catalog and benchmark repairs are in `platform/catalog.rs`, `platform/catalog_tests.rs` and `model_bench.rs`. Lifecycle repairs are in `platform/execution/pipeline.rs`, `platform/residency.rs`, `platform/raw_models.rs` and `proxy/mod.rs`. Research fixes are in the MCP research transport, source accounting and grade evaluation. Verification results are recorded below.

Raw explicit unload intentionally uses its own bounded control pool and per-model eviction ownership. It bypasses the backend load mutex so it can free memory while a cold load waits. Treat that as a documented control-path exception, not evidence that requests are uncoordinated. Its model still must be idle, and cancellation/absence observations remain explicit.

## Ratings

Scale: 1 missing/fragile, 2 partial, 3 useful with important gaps, 4 strong foundation, 5 broadly validated and operationally mature. These are engineering judgments, not benchmark results. Initial scores come from the corresponding areas of the original resource audit; a dash marks an area without its own original score. Current scores account for this review's repairs and outstanding validation limits.

| Area | Initial / 5 | Current / 5 | Why it is not 5 |
|---|---:|---:|---|
| Rust architecture and ownership | 4 | 4 | Strong RAII and bounded state; physical execution remains outside Rust's authority |
| Memory admission | 3 | 4 | Shared forecasts and readiness wakeups; real pressure behavior still needs hardware validation |
| Idle retention and eviction | 3.5 | 4 | Per-model leases and confirmed observations; physical cancellation and memory recovery remain engine-dependent |
| Many-model caching and metadata | 2.5 | 4 | Bounded memory, absolute expiry and bounded fetch concurrency; metadata remains a time-limited observation |
| Queueing and concurrent model use | 4 | 4 | Load/decode leases and readiness wakeups; no general calibrated busy/cold scheduler or locality optimization |
| CPU/GPU optimization | 3 | 3.5 | Observed-placement constraints and limited completion prediction; per-device reservations and speed superiority are unproven |
| Prompt/cache-aware flows | 3.5 | 4 | Compatible runner reuse and cache observations; no persistent KV service through stock Ollama |
| MCP discovery and context economy | 4 | 4 | Bounded inputs, previews and paged audits; compact receipts still need caller verification |
| Artifact-qualified model evidence | — | 4 | Digest checks and response validity gates; tag-based execution cannot provide atomic artifact attestation |
| Jobs, results and cancellation | 2.5 | 4 | No accepted-job replay and optional bounded durability; one authority, finite retention and physical-stop uncertainty |
| Research answer reliability | 2.5 | 3.5 | Better provenance and qualification; a successful lookup and strong grade still do not establish answer correctness |
| Timing and resource observability | 3 | 4 | Disjoint waits and observed lifecycle outcomes; upstream load duration does not isolate storage I/O |
| Local model recommendations | — | 3 | Small, task-specific trials; several comparisons and cookbook runs remain unfinished |
| Proven performance advantage | Unrated | Unrated | No controlled before/after workload or comparable end-to-end competitor measurement |

There is no overall average: reliability, quality and speed are not interchangeable. The main differentiator remains a local-agent execution layer combining resource control with compact evidence receipts. Queueing, caching, model swapping and MCP integration already exist elsewhere; the [competitor review](OLLAMA_BEFORE_AFTER_COMPETITORS_2026-11-01.md) contains the product-level evidence and references.

## Models: what the repository supports saying

The current `LOCAL_RIVALS.md` records small trials, not new measurements from this review:

- Decision classification: Strands recorded 53/56 in three rounds on that set.
- OCR: both tested models found the expected phrases on 30/30 rendered images; MiniCPM's recorded median was lower. This grader does not establish general OCR accuracy.
- Code search: embeddinggemma-2 recorded 9/10 first-place hits against nomic's 6/10, with a longer indexing time. The chunk counts also differed, so this does not isolate the embedding model alone.
- Coding: the small LFM3 recorded 21/20/20 of 25 versus Bonsai's 19/19/17. Parameter count did not predict the winner on these questions.
- Research-agent and several model-profile/cookbook comparisons remain incomplete. The saved find-code cookbook result includes an embedding-identity refusal, so it must not be counted as a passing end-to-end workflow.

Treat installed/resident labels in old documents as historical. This session could not reach Ollama, so no current inventory, placement or benchmark winner was inferred. Some model guides predate the newer rival results; use the recorded workload and its grading rules rather than treating a guide's default as universally best. No workflow here requires closing the user's editor.

## Verification

The final checks covered the changed working tree. Earlier successful external runs recorded in `HANDOFF.md` and the hardening report remain historical evidence, not results reproduced by this session.

| Check | Result in this session |
|---|---|
| `yarn typecheck` | Passed |
| `cargo fmt --all --check` and `git diff --check` | Passed |
| `RUSTC_WRAPPER= cargo clippy --offline --workspace --all-targets --all-features -j 2 -- -D warnings` | Passed |
| Focused MCP evidence, jobs, cancellation, embedding and identity regressions | 120 passed across six files, plus three targeted transport tests; these overlap the full suite |
| `yarn test:js` | 936 passed, 12 failed across four files; 11 reported loopback-listener `EPERM` errors and fixture timeouts. The full suite did not pass |
| `RUSTC_WRAPPER= cargo test --offline --workspace --no-fail-fast -j 2` | 434 passed, 203 failed, four ignored. Of 203 failure blocks, 202 explicitly report `Operation not permitted`; one asserts host memory capacity is available, but that observation was unavailable here |

Rust totals comprise CLI unit 14 passed; CLI integration two passed/three failed; core unit 326 passed/18 failed/one ignored; core integration 92 passed/182 failed/three ignored. Both new benchmark-response unit tests passed. The new catalog and lease regressions compiled but stopped at socket creation, before exercising their assertions. They still need a rerun with loopback access. The production release gate and physical CPU/GPU performance were not validated by this review. Cargo parallelism was capped at two; the attempted lower process priority was denied by the environment.

Source navigation included local Octocode structural search. Its LSP call failed with `executionFailed`; that was treated as a tool failure and direct source search was used, not as proof that symbols were absent. No models were pulled, run, unloaded or deleted by this review, and no Ollama settings were changed.

## Remaining boundaries

- No atomic tag-to-response guarantee when other clients can mutate tags or use Ollama directly.
- Metadata freshness and cached snapshot windows are bounded, not instantaneous global coherence.
- Durable recovery requires the same database, sibling `.job-key`, caller identity and topology; unknown outcomes are never automatically replayed. Backup those together. See [job recovery](DEFERRED_JOBS.md).
- Native MCP job polling cannot yet abort an in-flight GET. Cancellation dispatch begins immediately, but its acknowledgement can reach the caller only after that poll settles or its control timeout expires (30 seconds by default). Unknown acknowledgement does not mean the request was never delivered.
- Full cold/busy completion prediction, queue locality and exact per-device GPU reservations need measured workloads before policy expansion.
- Activation tracing is not part of the current Ollama/FreeLlama API. See [model introspection](MODEL_INTROSPECTION.md) for quantization commands, token probabilities and actual engine tracing options.
