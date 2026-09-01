---
name: freellama
description: "Use when delegating token-heavy work to local Ollama models through FreeLlama: grounded file research, image/OCR tasks, embeddings, bulk transforms, model selection or installation, context compaction, CPU/GPU placement, and FreeLlama/Ollama diagnosis. Also use to decide whether local delegation is worthwhile and how much verification its answer needs."
---

# FreeLlama

Operate FreeLlama as governed local-model delegation, not as a frontier-model replacement. MCP tools: `doctor`, `models`, `run_task`, `ollama_manage`, `ollama_delete`, `delegate_research`; `npx freellama tools` maps them to CLI equivalents.

Flow: `PREREQUISITE → DIAGNOSE → DECIDE → EXECUTE → VERIFY`

## Know who owns execution

| Owner | Responsibility |
|---|---|
| Calling agent | Decompose the requested goal, decide what is worth offloading, and launch independent MCP calls concurrently |
| Operator | Configure Ollama processes and endpoints, assign exact `--cpu-model` tags, approve pulls/stops/deletes, and change runtime settings |
| FreeLlama | Qualify installed models, choose only among eligible operator-assigned backends, enforce admission, forward a managed request, and compare the assignment with post-run `/api/ps` evidence |
| Ollama | Store/load runners, apply `num_gpu`, manage its request queue and same-model parallelism, and perform inference |
| OS, driver, accelerator runtime | Schedule the physical CPU/GPU work and memory |

FreeLlama is a governed router, not an unrestricted hardware scheduler. A CPU preference cannot
move an arbitrary tag: the operator must have assigned that exact model to the CPU Ollama process.
One managed call contains one task; only the calling agent knows which tasks are independent.

## Prerequisite and diagnose

1. Call `doctor`. FreeLlama does not install or replace Ollama; if Ollama is missing or unreachable, direct the operator to `https://ollama.com/download` or `ollama serve`, then stop—do not recommend or pull a model.
2. Read `doctor.machine.memory_bytes`, then `models{view:"installed"}` and `models{view:"resident"}`. Treat `execution.backend` as configuration and `execution.observation.processor` as physical evidence. A mismatch or partial placement warning invalidates feedback. For authenticated serve, set `FREELLAMA_AUTH_TOKEN_FILE` to the same permission-restricted file used at startup.
3. Check `/_freellama/v1/health` admission slots. Zero slots means queueing up to the configured wait, not a reservation; a 503 is load shedding, so retry or lower fan-out.

## Decide

| Delegate | Keep with the orchestrator |
|---|---|
| Grounded lookup over >~1k source tokens, OCR, embeddings, bulk transforms, privacy/rate-limited work | Tiny lookups already in context, architecture/review judgment, ambiguous synthesis |

Prefer deterministic search when an identifier is known; embeddings are for no-keyword similarity. Treat every measured number as one-machine evidence, not a default. Models at or below 12B were unreliable for broad research in the recorded trials.

Before offloading, map the work to `completion`, `coding`, `code_repair`, `tools`, `browser`,
`vision`, `embedding`, or `long_context`, plus required capabilities and context. Prefer a
qualified installed model; never infer inventory from a model family name or this skill's examples.

## Execute one flow

### A — grounded files

Call `delegate_research{question,workspacePath,model?,adapter?,executionPreference?,minPlacementEvidence?,agent?}` with one narrow lookup over 1–5 source files. It needs `serve`: every model turn is a managed `coding` task with admission and physical-placement receipts. Keep the path inside `FREELLAMA_MCP_ALLOWED_ROOTS`; Bash is the measured default, Octocode is opt-in for structural/LSP search. Read `verification`, full `citations[]`, `contextManagement`, and `execution.receipts`. Retry `isError` once, then do the work yourself. Load `references/task-delegation.md` for the measured boundary and `references/context-management.md` before tuning compaction.

### B — supplied content or vision

Call `run_task{task,prompt|messages,images?,model?,executionPreference?,minPlacementEvidence?,keepAlive?,minConfidence?,preview?}`. It has no file access. `preview:true` is free; `minConfidence` gates quality evidence. `minPlacementEvidence:"observed"` additionally refuses cold, mixed, or mismatched processor placement; warm once with `"configured"`, inspect the receipt, then require `"observed"`. Images are base64 without a data-URI prefix and require an explicitly trialled vision model. GLM-OCR needs a model-specific repetition guard: `options.stop:["```"]` handles fence tails; add `"\n"` only for a one-line transcription contract. Load `references/model-selection.md` before model choice.

### C — embeddings

Batch `run_task{task:"embedding",input:[...]}` and leave `returnEmbeddings:false` unless code, not the orchestrator, needs vectors. Use embeddings for grouping, dedup candidates, classification, and similarity—not identifier search. `examples/local-rag.sh` is the runnable pattern.

### D — choose or install a model

Ask for missing workload/modality, quality, latency, context, privacy, download, disk, and memory constraints. Diagnose the host, prefer qualified installed models, then search `models{view:"library"}` by family and inspect exact pullable tags with `models{view:"detail",model}`. Present at most two evidence-backed candidates. Ask approval for one exact tag and size before `ollama_manage{action:"pull"}`; discovery never grants installation permission. Load `references/model-selection.md`, then `references/ollama-config.md` for fit.

### E — diagnose a failure

Run `doctor`, resident models, then `scripts/check.sh` (read-only; exit 0 means healthy). Load `references/troubleshooting.md` for symptom routing. Never treat a 503, confidence refusal, CPU spill, or proxy/serve 404 as the same failure.

## Verify every result

| Verdict | Meaning | Action |
|---|---|---|
| `accept` | measured strong model, lookup-shaped task, 1–5 successful grounded calls | use with citations |
| `verify` | unmeasured/weak model, judgment-shaped task, or outside the measured call envelope | independent frontier-tier check |
| `escalate` | unusable model or zero successful calls | discard the answer |

Keep `pinnedOverflow:"error"`; clipping the system prompt/question requires explicit human acceptance. Accept adaptive placement feedback only when `execution.observation.status:"verified"`; for `keepAlive:"0"`, also inspect the observe-then-unload `execution.lifecycle`. Persist model-specific token calibration and bounded placement feedback; use ephemeral modes only for disposable tests. An assigned CPU backend can still execute an MLX model fully on GPU. Never co-resident two large models without memory arithmetic. Never delete `~/.ollama/models` files directly; `ollama_delete` needs exact-tag approval in the current conversation. Operators own endpoints, CPU tag assignments, Ollama settings, pulls, stops, and deletes.

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

Use current `doctor` output, installed server behavior, and upstream Ollama docs before changing runtime settings. Read one routed reference at a time; detailed trial transcripts live under `assets/evidence/` and should be opened only when auditing a quantitative claim.
