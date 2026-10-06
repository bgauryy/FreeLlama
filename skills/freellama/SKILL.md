---
name: freellama
description: "Use when delegating token-heavy work to local Ollama models through FreeLlama: grounded file research, image/OCR tasks, embeddings, bulk transforms, model selection or installation, context compaction, CPU/GPU placement, and FreeLlama/Ollama diagnosis. Also use to decide whether local delegation is worthwhile and how much verification its answer needs."
---

# FreeLlama

Operate FreeLlama as governed local-model delegation, not as a frontier-model replacement. The nine MCP tools and their request shapes are listed in the [MCP reference](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md#tools).

Flow: `PREREQUISITE → DIAGNOSE → DECIDE → EXECUTE → VERIFY`

## Know who owns execution

The calling agent owns decomposition, prompts, and verification; the operator owns endpoints, exact
`--cpu-model` assignments, and pull/stop/delete approval; FreeLlama routes, admits, and observes;
Ollama and the OS execute. The full rule is in [Architecture](https://github.com/bgauryy/FreeLlama/blob/main/docs/ARCHITECTURE.md#responsibility-boundary).
A CPU preference cannot move an arbitrary tag, and one managed call contains one task.

## Prerequisite and diagnose

1. Call `models{view:"installed"}`, then `models{view:"resident"}`. Call `doctor` when `models` errors or Ollama is unreachable. FreeLlama does not install or replace Ollama; if Ollama is missing or unreachable, direct the operator to `https://ollama.com/download` or `ollama serve`, then stop—do not recommend or pull a model.
2. When memory fit matters, read `doctor.machine.memory_bytes`. Treat `execution.backend` as configuration and `execution.observation.processor` as physical evidence. A mismatch or partial placement warning invalidates feedback. For authenticated serve, set `FREELLAMA_AUTH_TOKEN_FILE` to the same permission-restricted file used at startup.
3. Check admission slots with `doctor{view:"status"}` (HTTP clients: `/_freellama/v1/health`). Zero slots means queueing up to the configured wait, not a reservation. Read structured refusal codes: queue saturation returns 429, while resource/deadline failures have their own receipts.

## Decide

| Delegate | Keep with the orchestrator |
|---|---|
| Grounded lookup over >~1k source tokens, OCR, embeddings, bulk transforms, privacy/rate-limited work | Tiny lookups already in context, architecture/review judgment, ambiguous synthesis |

Prefer deterministic search when an identifier is known; embeddings are for no-keyword similarity. Treat every measured number as one-machine evidence, not a default. Use the exact model’s workload evidence; parameter count does not qualify a research result.

Ordinary chat can omit `task` and use the `completion` default. Choose `coding`, `code_repair`,
`tools`, `browser`, `vision`, `embedding`, or `long_context` when that routing profile fits; callers
own prompts and system messages. Add capability/context constraints when needed. Prefer a
qualified installed model; never infer inventory from a model family name or this skill's examples.

## Execute one flow

### A — grounded files

Call `delegate_research{question,workspacePath,model?,adapter?,agent?}` with one narrow lookup over 1–5 source files. It needs `serve`: every model turn is a managed `coding` task with admission and physical-placement receipts. Keep the path inside `FREELLAMA_MCP_ALLOWED_ROOTS`; Bash is the measured default, Octocode is opt-in for structural/LSP search. Read `verification`, full `citations[]`, `contextManagement`, and `execution.receipts`. Retry `isError` once, then do the work yourself. Load `references/task-delegation.md` for the measured boundary and `references/context-management.md` before tuning compaction.

### B — supplied content or vision

Preview with `run_task{preview:true,task?,model?,executionPreference?,minPlacementEvidence?,minConfidence?}` and routing constraints only. Execute separately with `prompt` or `messages`, plus applicable runtime controls. `run_task` has no file access; `minConfidence` gates quality evidence. `minPlacementEvidence:"observed"` refuses cold, mixed, or mismatched processor placement. Warm once with `"configured"`, inspect the receipt, then require `"observed"`. Images are base64 without a data-URI prefix and need an explicit trialled vision `model` or an operator `[policies.vision]` model; a capability tag alone is refused. If `output_check.repetition_loop` appears, the model looped to the output cap: the answer text shows one copy, treat it as low confidence. Choose output limits and stop sequences from the tested output contract. Load `references/model-selection.md` before model choice.

### C — embeddings

Batch `run_task{task:"embedding",input:[...]}` and leave `returnEmbeddings:false` unless code, not the orchestrator, needs vectors. Use embeddings for grouping, dedup candidates, classification, and similarity—not identifier search. `examples/local-rag.sh` is the runnable pattern.

For work that may wait, use `run_task{defer:true,timeoutSeconds,priority,...}` and inspect its ID with `task_jobs{action:"get",jobId}`. `timeoutSeconds` includes discovery, queueing, loading, and inference. `task_jobs{action:"cancel",jobId}` waits for local permit release; it does not prove Ollama stopped decoding. Jobs and bounded results are process-local and expire; they are not resumed after restart. Preview accepts routing fields only: omit payloads, priorities, deadlines, and `defer`, then make a separate execution call.

### D — retain history or prepare a runner

Use `scope{action:"create",messages}` for bounded process-local history; `session` stores only
model affinity. Pass `scopeId` and `scopeRevision` with new input to `run_task`, then use its
returned revision. Fork at the expected revision before independent parallel branches. Read history
only with `includeMessages:true`; failures and cancellations do not append. Use
`run_task{warm:true,model,contextTokens,keepAlive?}` to load an exact installed runner profile through the
same admission and fit checks. Explicit `keepAlive` wins over finite adaptive retention. Load
`references/context-management.md` for scope limits and cache ownership.

### E — choose or install a model

Ask for missing workload/modality, quality, latency, context, privacy, download, disk, and memory constraints. Diagnose the host and prefer qualified installed models. Search `models{view:"library"}` for families, then inspect pullable tags with `models{view:"library",model:"FAMILY"}`. The `detail` view inspects an installed model. Present at most two evidence-backed candidates. Ask approval for one exact tag and size before `ollama_manage{action:"pull"}`; discovery never grants installation permission. Load `references/model-selection.md`, then `references/ollama-config.md` for fit.

### F — diagnose a failure

Run `doctor`, resident models, then `scripts/check.sh`. Exit 0 means required service checks passed; inspect warnings and resource admission before executing a task. Load `references/troubleshooting.md` for symptom routing. Never treat a 503, confidence refusal, CPU spill, or proxy/serve 404 as the same failure.

## Verify every result

| Verdict | Meaning | Action |
|---|---|---|
| `accept` | measured strong model, lookup-shaped task, 1–5 successful grounded calls | use with citations |
| `verify` | unmeasured/weak model, judgment-shaped task, or outside the measured call envelope | independent frontier-tier check |
| `escalate` | unusable model or zero successful calls | discard the answer |

Keep `FREELLAMA_AGENT_PINNED_OVERFLOW=error`; clipping the system prompt/question requires explicit human acceptance. Accept adaptive placement feedback only when `execution.observation.status:"verified"`; for `keepAlive:"0"`, also inspect the observe-then-unload `execution.lifecycle`. Persist model-specific token calibration and bounded placement feedback; use ephemeral modes only for disposable tests. An assigned CPU backend can still execute an MLX model fully on GPU. Never co-resident two large models without memory arithmetic. Never delete `~/.ollama/models` files directly; `ollama_delete` needs exact-tag approval in the current conversation. Operators own endpoints, CPU tag assignments, Ollama settings, pulls, stops, and deletes.

## Routes and deterministic helpers

| When / why | Load or run |
|---|---|
| Offload boundary, result fields, measured economics | `references/task-delegation.md` |
| Model choice, vision trial, confidence evidence | `references/model-selection.md` |
| Context estimator, compaction, pagination, pinned overflow | `references/context-management.md` |
| Memory, KV cache, resolved `OLLAMA_*` defaults | `references/ollama-config.md` |
| CPU/GPU preference, admission, feedback | `references/resource-routing.md` |
| Symptom → cause → fix | `references/troubleshooting.md` |
| Proxy vs full serve routes | `references/proxy-vs-serve.md` |
| Retry, timeout, restart behavior | `references/reliability.md` |
| Safe disk cleanup | `references/disk-cleanup.md` |
| Qwen 27B evidence or profiling another tag | `references/model-profile-qwen3.8-27b-mlx.md` |
| Human health audit without an agent | run `scripts/check.sh` |
| Minimal local vector search example | run `examples/local-rag.sh` |

Use current `doctor` output, installed server behavior, and upstream Ollama docs before changing runtime settings. Read one routed reference at a time; detailed trial transcripts live in the repository under `docs/dev/evidence/` and should be opened only when auditing a quantitative claim.
