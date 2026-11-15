# FreeLlama assessment

**Overall: 7/10 for bounded local work on a shared workstation.** FreeLlama has a coherent Rust
management core, substantial deterministic contracts and useful local measurements. The largest
remaining gap is evidence that local answers and tool calls are reliable across representative
tasks and machines. The second engine works for a deliberately small text surface; release and
hardware qualification are incomplete.

These ratings were established on **2026-11-14** and reviewed again on **2026-11-15**, including
unreleased changes. The [end-to-end Ollama review](OLLAMA_REVIEW.md) records the follow-up research,
feature coverage, implemented repairs and current verification. Scores remain unchanged because
regression fixes do not supply missing representative model or hardware evidence.
Astra reviewed runtime architecture, APIs and production behavior; two Sol reviews covered
calibration/reliability and CI/Windows/maintenance. The orchestrator reconciled their findings and
reviewed the entry points, agent instructions and documentation. Scores are engineering judgments,
not measured accuracy percentages, a competitive ranking, or a promise that unreleased features
are already in npm. The overall score weighs the intended workstation use; it is not an arithmetic
average of implemented features and optional future work.

## Rating scale

| Score | Meaning |
|---|---|
| 0 | Absent |
| 2 | Design or scaffolding only |
| 4 | Narrow implementation or evidence |
| 6 | Working implementation with meaningful contracts and limited live proof |
| 8 | Broad repeatable contracts, clear ownership and bounded live evidence; qualification gaps remain |
| 10 | Representative repeated workloads, supported platforms and failure modes qualified in production |

Half points distinguish a substantial remaining qualification gap. A high architecture score does
not compensate for weak answer-accuracy evidence.

## The five implementation priorities

| Priority | Rating | Implemented and demonstrated | What keeps it below 9–10 |
|---|---:|---|---|
| 1. CI and test coverage | **7.5/10** | [Three-OS workflow](../.github/workflows/ci.yml), strict local Rust lint, build/type checks, 781 passing Rust tests, 1,474 passing JavaScript/evaluation checks and 51 packaging contracts | Hosted lanes have not been observed; Windows selects portable JavaScript contracts and does not qualify POSIX research/coder adapters. Full live integration, hardware and all-platform release gates remain separate. [Validation scope](TESTING.md) |
| 2. Second engine: llama.cpp | **7/10** | Explicit loopback `name/model` text execution shares Rust admission and resource reservations; nine engine contracts and a [retained live smoke](assets/llamacpp-managed-smoke-2026-11-14.json) with Ollama unavailable | No automatic discovery/selection, streaming, tool calls, embeddings, vision, lifecycle management or observed placement for this engine. The caller supplies a bounded context/output/memory configuration. [Engine contract](LLAMACPP.md) |
| 3. Per-machine calibration | **7/10** | Schema-2 reports bind throughput to host/hardware, local endpoint and artifact digest; job selection consumes the qualifying Rust route before catalog fallbacks | Throughput is not correctness. Capable jobs and OCR still need task-quality evidence; calibration is Ollama-specific and lacks a representative multi-machine study. Runtime/settings changes require an operator rerun. [Selection](MODEL_SELECTION.md#turn-measurements-into-routing-evidence) |
| 4a. Accuracy of `check:` lines | **5/10** | A reproducible 30-case labeled corpus retains misses and false alarms; tool arguments receive bounded structural validation. Numeric matching and malformed tool-call handling were repaired in the follow-up | Synthetic, implementation-visible examples are regression evidence. The original source-risk recall was 25%; the repaired numeric-token checker reaches 37.5% on the same exposed cases. Injection-risk recall remains 50%, dropped-value recall 33.3%. Clean checks do not prove correctness. [Versioned corpus and limits](RELIABILITY.md#accuracy-of-result-checks) |
| 4b. Public tool-calling evidence | **4/10** | Exact model identities, prompts, settings, raw turns and graders are retained; two models ran seven cases once, scoring 6/7 and 5/7 strictly | Ten other installed audit tags remain unmeasured. The public single-trial suite cannot rank models or estimate general reliability; the observed failures were formatting. [Table and transcripts](RELIABILITY.md#measured-on-2026-11-14) |
| 5. Panic/lock audit and large files | **8/10** | Focused poison, sampler-panic and subprocess-cleanup coverage; production lint census found no `unwrap_used` or `await_holding_lock` hits; large production files split into helpers/tests | Thirty explicit `expect` sites were counted, not all formally proved. Lock ordering, contention and OS thread exhaustion need broader qualification. Large test modules remain. [Audit](RUST_RESOURCE_AUDIT.md) |

The earlier approximate count of 426 `unwrap` calls mixed production and test code. It was a
review starting point, not a production panic count; retaining clear test assertions is appropriate.
Likewise, splitting files improves review boundaries without proving runtime correctness.

## Other product areas

| Area | Rating | Evidence and remaining constraint |
|---|---:|---|
| Architecture and ownership | **8/10** | Routing and resource policy stay in Rust; callers own prompts, dependencies and verification. Engine-specific operations are now explicit, but discovery, warming and feedback remain centered on Ollama. [Architecture](ARCHITECTURE.md) |
| Resource scheduler | **8/10** | Bounded weighted queues, reservations, cancellation and resource waits have contracts; waiting tasks yield execution units while retaining bounded queue tickets. Budgets and leases are process-local, so multiple authorities do not share them. Physical GPU/host contention needs broader testing. [Monitoring](MONITORING.md), [production boundary](PRODUCTION.md) |
| APIs and agent ergonomics | **8/10** | MCP, shell, operator CLI and HTTP reuse core contracts, receipts and explicit preview. Different engines and protocol doors support different controls; the agent CLI and global readiness still require Ollama. [API](API.md), [agent CLI](AGENT_CLI.md) |
| Installation and packaging | **8/10** | Node 22 npm/npx installs select one of five prebuilt native packages; standalone operators can use the Rust binary without Node. Source builds need Rust. Optional dependencies must remain enabled; Intel macOS and musl Linux lack prebuilt targets, and current all-target packed-install qualification is outstanding. [Build/install](BUILD_AND_NOTES.md), [packaging checks](TESTING.md#verify-release-packages) |
| Windows support | **6.5/10** | Bounded CIM physical-memory sampling and `require-memory` defaults have invalid/overflow/zero fixtures. Native Rust and selected portable JavaScript contracts are configured in CI but unobserved here; POSIX research/coder paths are not qualified for native Windows. CPU load, pressure, swap, thermal and GPU observations remain unknown. [Windows audit](RUST_RESOURCE_AUDIT.md#windows-memory-contract), [CI scope](TESTING.md) |
| Research/coder boundaries | **7/10** | Read-only lookup, bounded observations and caller verification are explicit; coder is opt-in, returns a patch from an isolated copy and permits only selected commands. Real coding-outcome and sandbox-platform evidence is limited; this remains substantial scope alongside the resource manager. [Research recipe](cookbooks/research-agent.md) |
| Performance evidence | **7/10** | Retained single-machine data covers overhead, concurrency, cold observations and selected workflows. It does not predict other hardware or broad answer quality; the historical workflow denominator is incomplete. [Benchmark](BENCHMARK.md) |
| Deployment and recovery | **6.5/10** | Loopback/auth boundaries, bounded durable receipts, unknown-outcome recovery and refusal-before-execution fallback are documented and tested. Managed-engine changes now conflict with retained input identity without moving the lookup namespace. No complete current production gate or hardware matrix is recorded. [Production](PRODUCTION.md), [durable jobs](DEFERRED_JOBS.md) |
| Documentation | **8/10** | Guides, recipes, agent skill, package READMEs and source examples were reconciled with code, with links and example schemas checked. Historical external claims remain dated; this was not a fresh audit of every upstream URL or a clean-machine usability study. [Index](README.md) |

The research/coder boundary rating measures containment and authority, not whether those adapters
belong in the default product. Making them a separately installed feature remains a scope and
maintenance decision; they have not been extracted by this change.

## Availability and ambitious options

| Capability | Status | Consequence |
|---|---|---|
| Explicit fallback before execution | Implemented | Up to four caller-named candidates; attempts and refusal reasons stay in the receipt. Ambiguous inference outcomes are not replayed. [Fallback rules](FEATURES.md#fallback-chains) |
| Engine-neutral automatic failover | **Not implemented (0/10)** | Shared admission is a foundation, but engine discovery, capabilities, health/readiness and takeover are not a complete neutral scheduler. A second text adapter alone does not provide availability failover. |
| Local-first automatic cloud escalation after failed checks | **Not implemented (0/10)** | The caller decides whether to rerun locally or use a hosted model. Existing external-upstream transport does not supply this policy, data-transfer authorization, cost limits or correctness evaluation. |
| Multi-machine pool | Deliberately excluded | One coordinating FreeLlama authority per inference topology remains the supported resource model. |

Failover safety is stronger than failover availability: refusing to replay an uncertain POST avoids
duplicate work, but does not make a failed request succeed. Do not describe the current fallback
chain as automatic cross-engine recovery.

For retained durable keys, changed managed-engine mappings now produce an explicit conflict.
Original receipts remain readable; inspect them before intentionally starting work with a new key.
The [migration and conservative invalidation rules](DEFERRED_JOBS.md#idempotency) are explicit.

## Next work, in order

1. **Observe CI and complete release qualification** (small to medium). Run the configured lanes,
   packed installs on supported targets and relevant live tiers; retain their versions, results and
   skips. Configuration and local success are not cross-platform qualification.
2. **Broaden correctness evidence** (large). Add unseen, representative factual/extraction cases
   with independent labels; report precision/recall and abstentions. Repeat tool cases across exact
   model artifacts, context lengths and machines; include semantic grading alongside strict format
   grading and preserve every attempted denominator. This is the largest route to a stronger product.
3. **Extend the engine contract deliberately** (medium to large). Choose the next real client need
   such as streaming or tools, add a capability matrix and failure tests, and qualify it live before
   broadening default routing. Keep uncertain execution out of automatic replay.
4. **Validate calibration across machines** (medium). Pair host-local performance with digest-bound
   task-quality evaluations, document rerun triggers and measure whether picks improve actual jobs.
5. **Finish operational qualification and scope reduction** (medium). Measure lock contention and
   sustained load, exercise Windows CIM and supported drivers, and decide whether research/coder
   should install separately. Prebuilt packages already remove the end-user Rust requirement.

These are effort estimates. Automatic cloud escalation is optional future work after check quality
and explicit data-transfer/cost policy are established; a multi-machine pool is outside this plan.

## Historical documentation review and validation

The review covered the root README/changelog/agent instructions, every operator guide, all 15
cookbook pages, both Octocode instruction sheets, the FreeLlama skill and references, package
guides and README generation, the benchmark harness guide, and the example policy. Five cookbook
pages and the skill's output format required no semantic change. Raw measurement files and
generated evidence tables were retained; their scope and denominators were corrected in the guides.

Before the final 2026-11-14 documentation edits, the implementation's local macOS validation passed `yarn build`, `yarn typecheck`,
`yarn lint:rust`, `yarn test` and `yarn test:release`. That historical snapshot included 1,292 Vitest,
112 benchmark/grader and 10 reliability checks; one JavaScript test was skipped and four Rust
tests ignored. A read-only macOS host-resource smoke ran separately. The retained llama.cpp and
two-model tool-calling runs are bounded live evidence, not the full inference test suite.

For this documentation pass, all 18 cookbook schema/resource integration checks and all 31
cookbook JSON examples passed; the FreeLlama skill review reported no errors or warnings.
The two compact Octocode sheets were checked against installed version 20.2.0: 23 query examples
matched 12 local tool schemas, using only help/schema commands.
The JavaScript build refreshed the packaged guides, synchronized package READMEs matched their
source, and all 51 packaging contracts passed again. The repository-wide link pass checked 64
Markdown files, 589 local/repository links and 178 heading anchors without errors. No new model
inference or download was needed to update this assessment. External URLs were not rechecked.

The 2026-11-15 full baseline found two failures: the Octocode documentation had changed without
updating its embedded prompt copies. The follow-up removed that duplicate source through build-time
Markdown imports and restored the existing equality and 30-line checks. Current validation and
the freshly checked primary Ollama sources are recorded in the [follow-up review](OLLAMA_REVIEW.md).
