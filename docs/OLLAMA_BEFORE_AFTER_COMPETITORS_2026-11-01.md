# FreeLlama before and after: Ollama competitors and model coverage

Review date: 1 November 2026. This follows the [initial audit](OLLAMA_RESOURCE_AUDIT_2026-11-01.md) and [implemented completion-planning changes](OLLAMA_IMPLEMENTATION_PLAN.md).

Subsequent implementation: [correctness, memory and caching fixes](OLLAMA_HARDENING_2026-11-01.md). Ratings and open findings below describe this review's earlier snapshot.

FreeLlama now has a stronger resource-management foundation, especially runner compatibility, retained performance evidence and observable lifecycle outcomes. Its strongest position is bounded local execution for agents, combining task eligibility, workstation resource admission and execution evidence. Several products already do much more above Ollama in model administration, retrieval, semantic routing and application workflows. Those are useful integration targets and design references.

## Comparison boundary

“Before” is the working tree inspected in the initial audit, based on HEAD `e718d8ad7af6af6e2a05ee6d3fc075dec9e8216f` with substantial existing work. “After” credits the completed implementation slice described in the plan. It is not a comparison of two published releases or attribution of every staged change to this work. Audio, decision, file processing and nearest-neighbor features were already present or being developed separately.

Five parallel review tracks covered gateways, Ollama applications, workflow/IDE integrations, the close scheduling competitor `ollama-queue`, and current model-routing code. Local Octocode structural searches and LSP queries were used; incomplete LSP results during indexing were checked with source search. Competitor evidence comes from primary documentation and selected source, with limitations below. No competitor was deployed or benchmarked.

Read-only localhost requests were denied by this execution environment. Current installed tags, residency and backend version were therefore not verified. Existing repository measurements are historical evidence, not a fresh inventory or a before/after performance experiment. No models were pulled, warmed, unloaded or deleted for this review.

## Before and after ratings

The initial rubric is retained: **1** missing/fragile; **2** partial; **3** useful with significant gaps; **4** strong foundation; **5** broadly validated and operationally mature. Half-points express engineering judgment. Scores are not measured performance or market rankings. There is no overall average: correctness gaps cannot be offset by unrelated features.

| Area | Before / 5 | After / 5 | Evidence and remaining limit |
|---|---:|---:|---|
| Rust ownership and central policy | 4 | 4 | Clear core/agent/engine responsibilities; per-model lifecycle atomicity remains incomplete |
| Managed queue and fairness | 4 | 4 | Existing weighted priorities and anti-starvation remain; no new locality queue or cold/busy scheduling model |
| Memory admission correctness | 3 | 3.5 | Observation generations prevent stale samples after reservation release; raw accounting and per-device/global coordination gaps remain |
| Idle retention and eviction | 3.5 | 4 | Requested versus confirmed unload is explicit; speculative warming fails fast without eviction. Activity checks still need atomic lifecycle leases |
| CPU/GPU routing efficiency | 3 | 3.5 | Payload fit and global quality gates precede selection; qualified idle residents compare setup, prefill and decode. Busy/cold candidates remain conservative |
| Many-model learning and metadata scale | 2.5 | 3.5 | Bounded exact-profile histories replace destructive profile switching; full metadata refresh and backend failure coupling remain |
| Prompt/cache-aware flows | 3.5 | 4 | Stronger digest/context reuse checks and cache-aware measurement qualification; no persistent KV service or predicted prefix-hit benefit |
| MCP discovery and context economy | 4 | 4 | Existing previews, scopes and bounded results; compact receipts now expose planning evidence, but discovery was not redesigned |
| Job/result/cancellation semantics | 2.5 | 3.5 | Missing/oversized accepted-job results no longer cause automatic inference replay; retention, durable recovery and cancellation consistency remain |
| Research evidence correctness | 2.5 | 2.5* | No increase attributed to this implementation; separate research-adapter changes require their own acceptance evidence |
| Timing/resource observability | 3 | 4 | Disjoint admission timing, preserved engine load duration, stronger transition evidence and explicit estimates |
| Proven speed advantage | Unrated | Unrated | No controlled before/after or competitor workload measurement |

*The research score is carried forward for change attribution, not a certification of concurrent adapter work. A tool result marked for caller verification is not evidence that its answer is correct.*

The implemented planner compares the selected CPU and primary candidates only when both are idle, observably compatible residents with sufficient exact-profile phase samples. It requires a predicted advantage greater than 10%. Missing queue, load or cached-token evidence stays unknown. This is useful groundwork, not a general optimal scheduler or automatic tensor migration.

The implementation's typecheck, formatting and Clippy checks passed, with 49 focused MCP tests and 257 core unit tests passing. The full suite was not green in this environment: socket fixtures and a host-memory assertion encountered permission failures. See the implementation plan for exact commands and counts. No performance score is inferred from passing deterministic tests.

## Competitors actually using Ollama

“Not established” below means the inspected evidence did not establish a feature. It is not proof that every edition, extension or release lacks it. Documentation on moving branches may exceed released behavior.

| Project | What it adds above Ollama | Comparison and lesson |
|---|---|---|
| **ollama-queue** | Advertises priority/short-job scheduling, host-pressure gates, model-locality handling, recurring jobs, retries and dead-letter handling [Q1] | Closest newly identified scheduler overlap. Treat documentation claims as an evaluation target; executable scheduler source was not independently verified here |
| **Olla** | Native Ollama gateway, unified discovery, endpoint balancing and optional conversation affinity [O1–O4] | Stronger general endpoint gateway breadth. Its profile concurrency settings are explicitly metadata, not enforced admission. Borrow digest-aware discovery and affinity; preserve FreeLlama's resource budgets |
| **LiteLLM** | Native Ollama adapters, provider routing, fallbacks, response caches and MCP gateway [L1–L4] | Broad interoperability. Current deployment caps reject overflow; a separate beta priority scheduler exists. Neither fact establishes Ollama host-memory reservations |
| **Open WebUI** | Pull/update/create/delete flows, resident indicator, eject, multiple Ollama nodes, chat/RAG/tool UX [W1,W2] | Broader model-administration experience. Its documented balancing is random across matching model IDs. Borrow visible model states and update lifecycle |
| **AnythingLLM** | Workspace RAG, ordered model-routing rules, cached classifier decisions, sticky routes and MCP [A1–A3] | Semantic routing is already competitive. Borrow clear routing reasons; keep physical eligibility and admission separate |
| **Page Assist** | Browser context, multiple Ollama connections, model pull/delete, local retrieval and MCP controls [P1–P3] | Broader browser workflow. A conversation message queue does not establish a cross-application resource scheduler |
| **Dify** | Ollama tools, vision, embeddings, task classifiers and application/workflow composition [D1,D2] | Useful caller for managed execution. Its rerank integration points to a separate compatible service; it is not evidence of native Ollama reranking |
| **Flowise** | Chat/embedding nodes, agent graphs, MCP and evaluation [F1–F3] | Keep workflow ownership there. Reviewed built-in evaluation is Cloud/Enterprise; an explicit FreeLlama integration would need validation |
| **n8n** | Ollama chat/embedding nodes, external application automation, MCP and evaluation [N1–N3] | Strong integration surface. Preserve job identity through workflow retries and correlate individual items |
| **Continue** | Distinct chat, autocomplete, edit, apply, embed and rerank roles; native Ollama adapter [C1,C2] | Best role-separation precedent in this sample. Interactive coding and background indexing need different budgets |

`llama.cpp`, vLLM, SGLang, oMLX and related engines belong in a separate engine comparison. `llama-swap` is an orchestration reference, not evidence of using Ollama internals. Their paging, slot caches or sleep modes cannot be assumed accessible through Ollama. The initial audit covers those architectural comparisons.

The close scheduler comparison warrants caution: `ollama-queue`'s README and scheduling design disagree on priority direction. Source retrieval did not establish executable behavior, and advertised test counts do not prove reliability. FreeLlama should compare actual traces before claiming stronger scheduling or durability.

Open WebUI's update workflow highlights a lifecycle distinction: a new installed revision can coexist with an old loaded revision until unload. FreeLlama should surface installed digest versus resident digest and invalidate qualification appropriately. Its ownership rules should remain narrower than an administrative “eject on every node” operation. [W1]

Continue explicitly discourages its general-LLM reranking path with local Ollama because of excessive parallel requests. Bounded admission could control that fan-out, but would not make an ordinary LLM equivalent to a specialized reranker or establish acceptable latency. [C3]

## Model coverage and remaining correctness gaps

This reviews the repository's model families and routing contracts. It does not certify every Ollama tag or model conversion.

| Family | Current FreeLlama path | What still needs validation or repair |
|---|---|---|
| Text, coding and extraction | Managed chat with Completion eligibility and payload-context filtering | Task quality belongs to an exact evaluated revision; advertised capability alone is insufficient |
| Tool use | Tools requirement derived from supplied tools and relevant task classes | Schema compliance, tool correctness and multi-step quality require workload tests and caller verification |
| Thinking | Policy defaults plus caller thinking controls | Explicit thinking does not derive a Thinking requirement; accepted effort strings are hardcoded, while current Ollama docs describe model-defined values |
| Vision/OCR | Image inputs derive Vision; automatic image work requires vision qualification | Real vision-head support, image token cost and OCR output quality must be tested for the exact package |
| Text embeddings | Managed embed calls; bounded sequential slices for large requests | Slices do not pin and verify one embedding identity; vector dimensions, cardinality and finite values need validation |
| Multimodal embeddings | File adapter can construct image/audio embedding items | Eligibility currently derives Embedding without independently proving those input modalities |
| Decision models | Decision capability and `/v1/systemone`; separate request contract | Model-specific platform/endpoint restrictions; media handling must distinguish image and audio support |
| Speech/transcription/translation | Explicit-model MCP audio calls through raw audio endpoints | This is not the same managed automatic model-selection contract as chat/embeddings |
| Rerank, video, image generation | No general managed contract established by this review | A model name or upstream family claim does not prove a supported endpoint or payload path |

Concrete source anchors:

- `packages/mcp/src/tools/run-task.ts`, `embedInSlices`: each slice repeats the original request, concatenates vectors and keeps the last receipt. An automatically selected different model can silently change embedding space. Equal vector length does not prove compatibility.
- `packages/mcp/src/nearest.ts`, `cosine`: assumes compatible vector lengths and values. Validate input before ranking.
- `packages/rust-core/src/platform/catalog.rs`, `catalog_model`: attaches benchmark and qualification records by mutable model name. The new performance-history digest checks do not repair this separate quality-policy binding.
- `packages/rust-core/src/platform/catalog.rs`, `snapshot_catalog`/`fetch_catalog`: 30-second full metadata caching and propagation of either backend's discovery error remain. One unavailable CPU endpoint can block unrelated managed primary work.
- `packages/rust-core/src/platform/execution/request.rs`, `apply_request_options`/`task_has_images`: thinking overrides and media detection need a complete capability contract. The current thinking validator accepts booleans or low/medium/high; it does not use the selected model's advertised effort values.

Current catalog examples reinforce why roles matter. `strands-decider` is a decision model using a separate endpoint, with an Apple-silicon/MLX requirement documented for its Ollama package. It is not a generic CPU helper or chat replacement. `lfm3-audio` advertises speech and tools but its own page discourages agentic coding and complex multi-step reasoning. [M1,M2] These are examples, not pull recommendations.

Registry cards can also disagree with their own detailed model documentation about context. Use installed model metadata, exact engine/version support and observed effective context before admission. Download size is not total runtime memory; weights, KV/cache, execution buffers and concurrency all matter. CPU and GPU on Apple silicon share physical RAM.

## What our existing model results actually show

These are small historical runs from this repository on an Apple M2 Pro with 32 GB unified memory and reported Ollama 0.40.13. They are not competitive benchmarks or fresh tests of the completed scheduler changes.

| Model and workload | Recorded result | Interpretation |
|---|---|---|
| `nomic-embed-text:v1.5`, classification | 18/20 | Promising bounded classification example; requires task-specific thresholds |
| `strands-decider:2b-nvfp4`, classification/scoring | 18/20 | Useful specialist; scoring mistakes remain |
| `glm-ocr:latest`, OCR | 30/30 expected-phrase checks | Grader did not penalize repeated text; outputs include repetition and large latency outliers. This is not general 100% OCR accuracy |
| `gpt-oss:20b`, coding/research | 25 coding and 5 research tasks held; zero executed | Evidence of admission behavior, not zero model accuracy or proof the refusal was optimal |
| `phonon:en` → `lfm3-audio:2b-q4_K_M`, speech round trip | Mean word error 7.5% over five sentences; one sentence about 38% | Small end-to-end speech sample; insufficient for broad transcription quality claims |
| Multimodal screenshot retrieval | 5/5 descriptions matched | Tiny corpus; each query re-embedded the five images, so persistent-index efficiency was not measured |
| Nomic versus `embeddinggemma-2`, code retrieval | Both 2/3 queries; one latter request held | Different execution outcomes and tiny sample prevent ranking general retrieval quality or speed |
| `lfm3-audio:2b-q4_K_M`, Octocode research | 4/8 local-code questions; 2/6 GitHub questions correct | Does not qualify the model for autonomous repository research. Preserve escalation and caller verification |

Sources: [LOCAL_TEST](../LOCAL_TEST.md), [LOCAL_MEDIA](../LOCAL_MEDIA.md), [LOCAL_OCTOCODE](../LOCAL_OCTOCODE.md). Task examples and grading rules should travel with any quoted result.

## What to build next while relying on Ollama

| Priority | Change | Acceptance evidence |
|---|---|---|
| 1 | One embedding-space identity across an entire operation and stored index: exact model/digest, dimensions and relevant options; validate every slice and aggregate receipts | Forced route/tag change cannot silently mix vectors; malformed/partial batches fail explicitly |
| 2 | Digest-bound task qualification and complete modality requirements, including model-defined thinking controls | Updating a tag invalidates old quality evidence; unsupported audio/image/thinking inputs are rejected before load |
| 3 | Cache static metadata by endpoint/digest and isolate backend failures; expose installed versus resident revision | Healthy pinned work survives another endpoint's outage; large catalogs avoid repeated full `/show` sweeps |
| 4 | Shared per-model lifecycle leases across managed work, raw traffic and eviction | A new request cannot race an idle check into unloading its runner; cancellation releases ownership |
| 5 | Opt-in durable deferred jobs with idempotency keys, attempt history and explicit unknown outcomes | Restart recovery preserves accepted job identity; ambiguous execution is not automatically replayed. Persistence does not imply exactly-once execution |
| 6 | Measure before broadening completion planning: cold loads, busy queues, contention and bounded locality bursts | Paired workloads improve useful throughput or p95 latency without quality loss, starvation or memory regressions |
| 7 | Publish tested integrations for an IDE and an Ollama application/workflow client | Streaming, tools, embeddings, cancellation, error handling and managed-versus-raw behavior are explicit |

Use Rust for bounded state, cancellation-safe RAII guards, monotonic deadlines, checked resource arithmetic, explicit lifecycle state and controlled asynchronous concurrency. Keep tensor allocation, execution kernels, token batching, KV management and device offload in Ollama and its engine. Rust alone does not make inference faster.

For cache efficiency, retain stable caller prefixes and compatible runner affinity where beneficial; measure prefill savings rather than assuming a hit. Separate metadata caches, response caches, resident weights and engine KV. An optional exact embedding-result cache needs digest/options identity and bounded storage. A scope's saved message history is not a saved KV cache.

Keep one coordinating authority per topology. A client using raw compatibility routes does not automatically receive managed model selection; direct requests to Ollama bypass FreeLlama coordination. MCP is the bounded task interface, while clients own prompts, workflow dependencies, tool authority and final verification. Integrating their richer workflows is more valuable than recreating their entire UI.

Describe this as **model residency management** or **automatic model loading and eviction**. It is not literal operating-system virtual memory, page-fault-driven tensor paging or migration of a live GPU session to CPU.

## Measurement needed for a defensible performance rating

Compare direct Ollama, the pre-change FreeLlama snapshot and the completed implementation on the same host. Pin exact model digests, backend versions, context, quantization, parallelism and workload. Then compare relevant competitors with equivalent workload constraints, distinguishing supported features from unconfigured ones.

Include warm interactive chat, cold load, A/B/A model alternation, chat plus embedding batches, long prompts, cancellation, memory pressure and endpoint failure. Record task correctness, useful completions, p50/p95 end-to-end latency, queue/resource/load/prefill/decode time, load/unload counts, peak RAM/VRAM, swap pressure and failed/cancelled work. Runner-cold and OS-page-cache-cold are different conditions. The current 10% prediction threshold is a routing rule, not a measured 10% speedup.

## Primary reading list

All external sources were reviewed on 1 November 2026. These are documentation/source references, not independent performance measurements.

- Q1: [ollama-queue repository](https://github.com/Kurat0r/ollama-queue); [scheduling design](https://github.com/Kurat0r/ollama-queue/blob/main/docs/plans/2026-03-04-queue-optimization-design.md); [recovery edge cases](https://github.com/Kurat0r/ollama-queue/blob/main/docs/plans/2026-03-13-edge-case-fixes.md). README/design-level evidence; executable scheduling behavior was not independently verified.
- O1: [Olla Ollama API](https://thushan.github.io/olla/api-reference/ollama/); [backend integration](https://thushan.github.io/olla/integrations/backend/ollama/). The API reference and integration example disagree on `/api/show`; verify the chosen release.
- O2: [Olla model unification](https://thushan.github.io/olla/concepts/model-unification/).
- O3: [Olla profile system](https://thushan.github.io/olla/concepts/profile-system/), including unenforced concurrency metadata.
- O4: [Olla sticky sessions](https://thushan.github.io/olla/concepts/sticky-sessions/).
- L1: [LiteLLM Ollama](https://docs.litellm.ai/docs/providers/ollama).
- L2: [LiteLLM routing](https://docs.litellm.ai/docs/routing); [separate priority scheduler](https://docs.litellm.ai/docs/scheduler).
- L3: [LiteLLM response caches](https://docs.litellm.ai/docs/caching/all_caches).
- L4: [LiteLLM MCP gateway](https://docs.litellm.ai/docs/mcp).
- W1: [Open WebUI Ollama management](https://docs.openwebui.com/getting-started/quick-start/connect-a-provider/starting-with-ollama/).
- W2: [Open WebUI prompt caching](https://docs.openwebui.com/features/chat-conversations/prompt-caching/); [MCP](https://docs.openwebui.com/features/extensibility/mcp/).
- A1: [AnythingLLM model router](https://docs.anythingllm.com/model-router/setup).
- A2: [AnythingLLM embedding configuration](https://docs.anythingllm.com/setup/embedder-configuration/overview).
- A3: [AnythingLLM external Ollama provider](https://docs.anythingllm.com/setup/llm-configuration/local/ollama); [MCP](https://docs.anythingllm.com/mcp-compatibility/overview).
- P1: [Page Assist Ollama management](https://docs.pageassist.xyz/features/ollama); [multiple providers](https://docs.pageassist.xyz/providers/ollama).
- P2: [Page Assist knowledge base](https://docs.pageassist.xyz/features/knowledge-base).
- P3: [Page Assist MCP](https://docs.pageassist.xyz/features/mcp).
- D1: [Dify Ollama plugin](https://marketplace.dify.ai/plugin/langgenius/ollama), reviewed page identifies version 1.0.3.
- D2: [Dify classifier](https://docs.dify.ai/en/cloud/use-dify/nodes/question-classifier); [MCP announcement](https://dify.ai/blog/v1-6-0-built-in-two-way-mcp-support).
- F1: [Flowise ChatOllama](https://docs.flowiseai.com/integrations/langchain/chat-models/chatollama); [embeddings](https://docs.flowiseai.com/integrations/langchain/embeddings/ollama-embeddings).
- F2: [Flowise Agentflow V2](https://docs.flowiseai.com/using-flowise/agentflowv2); [MCP](https://docs.flowiseai.com/tutorials/tools-and-mcp).
- F3: [Flowise evaluation](https://docs.flowiseai.com/using-flowise/evaluations).
- N1: [n8n Ollama implementation](https://github.com/n8n-io/n8n/blob/master/packages/@n8n/nodes-langchain/nodes/llms/LMChatOllama/LmChatOllama.node.ts); old Llama2-only documentation wording should not constrain the current adapter.
- N2: [n8n embeddings](https://docs.n8n.io/integrations/builtin/cluster-nodes/sub-nodes/n8n-nodes-langchain.embeddingsollama); [MCP](https://docs.n8n.io/build/ways-of-building-workflows/connect-to-n8n-mcp-server).
- N3: [n8n workflow evaluation](https://blog.n8n.io/introducing-evaluations-for-ai-workflows/).
- C1: [Continue model roles](https://docs.continue.dev/customize/model-roles/00-intro).
- C2: [Continue Ollama adapter](https://github.com/continuedev/continue/blob/main/core/llm/llms/Ollama.ts); [Ollama guide](https://docs.continue.dev/guides/ollama-guide).
- C3: [Continue reranking and local concurrency limitation](https://docs.continue.dev/customize/model-roles/reranking).
- M1: [Strands Decider Ollama package](https://ollama.com/library/strands-decider).
- M2: [LFM3-Audio Ollama package and limitations](https://ollama.com/library/lfm3-audio).
- M3: [Ollama generate controls](https://docs.ollama.com/api/generate); [running models](https://docs.ollama.com/api/ps); [FAQ](https://docs.ollama.com/faq).
