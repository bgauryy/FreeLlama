---
name: freellama
description: "Use when delegating token-heavy work to local Ollama models through FreeLlama: grounded file research, image/OCR tasks, embeddings, bulk transforms, model selection or installation, context compaction, CPU/GPU placement, and FreeLlama/Ollama diagnosis. Also use to decide whether local delegation is worthwhile and how much verification its answer needs."
---

# FreeLlama

Operate FreeLlama as governed local-model delegation, not as a frontier-model replacement. The eleven MCP tools and their request shapes are listed in the [MCP reference](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md#tools).

Flow: `PREREQUISITE → DIAGNOSE → DECIDE → EXECUTE → VERIFY`

## Connect through MCP or the CLI

Prefer the MCP server when the host has it; the same operations run from a shell through the CLI.

- **MCP:** `claude mcp add freellama -- npx -y @octocodeai/freellama-mcp-server` (other hosts: the same
  `npx` command in their JSON config). It starts `freellama serve` on `127.0.0.1:11435` at launch.
- **CLI:** `npx -y @octocodeai/freellama <command>`, JSON on stdout. `doctor`, `init`, `catalog`, `auth-token`,
  `serve`, `proxy`, `bench-all` and `policy` need no running serve; the managed commands (models, route, task,
  jobs, warm, unload, status, usage, session, scope, ready, config, machine) need one: the MCP server's, or `npx -y @octocodeai/freellama serve` kept running in
  the background. `--endpoint` or `FREELLAMA_SERVE_ENDPOINT` selects another one. The CLI has no
  decoding options (`num_predict`, `temperature`); use MCP or the HTTP API when you need them.

| MCP tool call | CLI command |
|---|---|
| `doctor`, `doctor{view:"status"}`, `doctor{view:"usage"}` | `doctor`, `status`, `usage --days N` |
| `models{view:"installed"}` / `{view:"resident"}` | `models` (each entry carries `resident`) |
| `run_task{preview:true,...}` | `route --task T --objective O [--min-confidence medium]` |
| `run_task{task,model,prompt,images,stopOnRepetition}` | `task --task vision --model TAG --image FILE --stop-on-repetition "PROMPT"` |
| `run_task{task:"embedding",input}` | `task --task embedding --input-file FILE` (one item per line) |
| `run_task{task:"decision",state,questions}` | `task --task decision --questions JSON\|@FILE "STATE"` (or `--input-file STATE_FILE`) |
| `run_task{task:"embedding",input,nearest:{query}}` | `task --task embedding --input-file FILE --nearest QUERY [--top N]` (one item per line) |
| `run_task{files}`, `audio{action,model,file or text}` | MCP only (the CLI reads `--input-file` and `--image`) |
| `ollama_manage{action:"warm",model,contextTokens}` | `warm --model TAG --context-tokens N` |
| `run_task{defer:true}`, `task_jobs` | `task --defer ...`, `jobs`, `jobs --id ID [--cancel or --remove]` |
| `ollama_manage{action:"unload",model}` | `unload --model TAG` (waits for the model's running tasks; never loads it first) |
| `doctor` (its `platform_health.readiness`) | `ready` (exit 0 only when serve can run work) |
| `read_response{file,section?,offset?,limit?}` | read the `response_file` JSON directly |
| `session{action:"create"}`, `scope{...}` | `session`, `scope create`, `scope get`, `scope fork`, `scope delete` |
| `models{view:"library"}`, `models{view:"detail"}`, `ollama_manage{action:"pull"}`, `ollama_delete`, `delegate_research` | MCP only |

## Know who owns execution

The calling agent owns decomposition, prompts, and verification; the operator owns endpoints, exact
`--cpu-model` assignments, Ollama settings, and pull/stop/delete approval; FreeLlama routes, admits,
and observes; Ollama and the OS execute. The full rule is in [Architecture](https://github.com/bgauryy/FreeLlama/blob/main/docs/ARCHITECTURE.md#responsibility-boundary).
A CPU preference cannot move an arbitrary tag, and one managed call contains one task.

## Prerequisite and diagnose

1. Call `models{view:"catalog", job}` (tested picks per job, whether each is installed and fits now) and pass that pick as `model`; add `models{view:"installed"}` or `models{view:"resident"}` only when the catalog does not answer. Call `doctor{view:"status"}` when a call fails or before a large batch. FreeLlama does not install or replace Ollama; if Ollama is missing or unreachable, direct the operator to `https://ollama.com/download` or `ollama serve`, then stop—do not recommend or pull a model.
2. When memory fit matters, read `doctor.machine.memory_bytes` (total RAM; `memory_kind` is `unified` only when RAM is shared with the accelerator; on discrete GPUs read VRAM from `/api/ps`, never derive it from RAM). Treat `execution.backend` as configuration and `execution.observation.processor` as physical evidence. A mismatch or partial placement warning invalidates feedback. For authenticated serve, set `FREELLAMA_AUTH_TOKEN_FILE` to the same permission-restricted file used at startup.
3. Check admission slots with `doctor{view:"status"}` (HTTP clients: `/_freellama/v1/health`). Zero slots means queueing up to the configured wait, not a reservation.
4. `run_task`, `delegate_research`, installed/resident models and health need `freellama serve` (the MCP server starts it on `127.0.0.1:11435`). `freellama proxy` only retries raw `/api/*` calls; a 404 on a control route means a proxy is answering, not serve.

## Decide

| Delegate | Keep with the orchestrator |
|---|---|
| Grounded lookup over >~1k source tokens, long-context retrieval, OCR, embeddings, bulk transforms, privacy/rate-limited work | Tiny lookups already in context, architecture/review judgment, ambiguous synthesis, hidden multi-step reasoning |

Prefer deterministic search when an identifier is known; embeddings are for no-keyword similarity (grouping, dedup candidates, classification), and a high cosine score is a candidate, never an equivalence or deletion verdict. Local code review is unreliable: verify every finding. For classification, routing or yes/no policy checks, a decision model (`capabilities` `decision`, through `run_task {task:"decision",state,questions}`) answers with probabilities in a fraction of a second; embeddings plus a few labeled examples per class are the cheapest classifier. Treat every measured number as one-machine evidence, not a default. Use the exact model's workload evidence; parameter count, family name, or a capability tag does not qualify a result.

Ordinary chat can omit `task` and use the `completion` default. Choose `coding`, `code_repair`,
`tools`, `browser`, `vision`, `embedding`, or `long_context` when that routing profile fits; callers
own prompts and system messages. Add capability/context constraints when needed. Prefer a
qualified installed model; never infer inventory from a model family name or this skill's examples.

## Execute one flow

### A — grounded files

Call `delegate_research{question,workspacePath,model?,adapter?,agent?}` with one narrow lookup over 1–5 source files. It needs `serve`: every model turn is a managed `coding` task with admission and physical-placement receipts. Keep the path inside `FREELLAMA_MCP_ALLOWED_ROOTS`; never widen it to `$HOME` or `/` for convenience. Bash is the measured default adapter, Octocode is opt-in for structural/LSP search. Read `verification`, successful call counts and full `citations[]` inline; `contextManagement`, `execution.receipts`, the evidence trail and every observation the model saw are in `response_file` (read one with `read_response{file,section}`, or pass `receipt:"full"`). Failed calls do not ground a claim. Retry `isError` once, then do the work yourself.

Context: the adapter budget is `contextTokens - outputTokens - safetyMarginTokens`. Older observations compact first and full tool output stays retrievable through pages. Tokens are estimated from the model's own bytes-per-token ratio that FreeLlama learns from Ollama's real counts (`token_counting:"platform_learned_ratio"`, `bytes_per_token_milli`); until a model has been measured the estimate falls back to `charsPerToken` with local calibration (`"local_chars_per_token"`). Either way it is an estimate, not an exact count. Keep `FREELLAMA_AGENT_PINNED_OVERFLOW` unset (error; system prompt and question are byte-preserved); `clip` is an operator env choice, never a call argument, and raising `contextTokens` or narrowing the task is safer. Tune per call with `agent`, deployment defaults with `FREELLAMA_AGENT_*`; confinement and read-only actions are not knobs.

### B — supplied content or vision

Preview with `run_task{preview:true,task?,model?,executionPreference?,minPlacementEvidence?,minConfidence?}` and routing constraints only. Execute separately with `prompt` or `messages`, plus applicable runtime controls. `run_task` reads local files only through `files` (under the server cwd, temp dir or `FREELLAMA_FILE_ROOTS`); `minConfidence` gates quality evidence. `minPlacementEvidence:"observed"` refuses cold, mixed, or mismatched processor placement. Warm once with `"configured"`, inspect the receipt, then require `"observed"`. Images are base64 without a data-URI prefix and need an explicit trialled vision `model` or an operator `[policies.vision]` model; a capability tag alone is refused, and vision claims need a real image trial. If `output_check.repetition_loop` appears, the model looped: the answer text shows one copy, treat it as low confidence. For a model known to loop, set `stopOnRepetition:true` to stop as soon as the output repeats (all generated text is kept, `output_check.stopped_early:true`); routing also ranks a model last for task kinds it keeps looping on. Choose output limits and stop sequences from the tested output contract.

### C — embeddings

Batch `run_task{task:"embedding",input:[...]}` and leave `returnEmbeddings:false` unless code, not the orchestrator, needs vectors.

When the client sends a `progressToken`, a single `run_task` reports its phases (queued, waiting for memory, loading, running) as progress and still returns the normal result. For work that may wait, use `run_task{defer:true,timeoutSeconds,priority,...}` and inspect its ID with `task_jobs{action:"get",jobId}`. `timeoutSeconds` includes discovery, queueing, loading, and inference. `task_jobs{action:"cancel",jobId}` waits for local permit release; it does not prove Ollama stopped decoding. Jobs and bounded results are process-local and expire; they are not resumed after restart. Preview accepts routing fields only: omit payloads, priorities, deadlines, and `defer`, then make a separate execution call.

### D — retain history or prepare a runner

Use `scope{action:"create",messages}` for bounded process-local history; `session` stores only
model affinity. Pass `scopeId` and `scopeRevision` with new input to `run_task`, then use its
returned revision. Fork at the expected revision before independent parallel branches. Read history
only with `includeMessages:true`; failures and cancellations do not append. Use
`ollama_manage{action:"warm",model,contextTokens,keepAlive?}` to load an exact installed runner profile through the
same admission and fit checks; warm at the context the tasks will send, or the first task reloads the
runner. Explicit `keepAlive` wins over finite adaptive retention. Limits and API shapes:
[Scopes and warming](https://github.com/bgauryy/FreeLlama/blob/main/docs/SCOPES_AND_WARMING.md).

### E — choose or install a model

Ask for missing workload/modality, quality, latency, context, privacy, download, disk, and memory constraints. Diagnose the host and prefer qualified installed models. Search `models{view:"library",query}` (ollama.com families plus Hugging Face GGUF repos; `capabilities` such as `embedding`, `audio` or `decision` filter), then inspect pullable tags and `fitsInMemory` with `models{view:"library",model:"FAMILY"}` or `model:"hf.co/<owner>/<repo>"` (each quant is a tag like `hf.co/<owner>/<repo>:Q4_K_M`); with `serve` down, do not guess a tag. The `detail` view inspects an installed model. Present at most two evidence-backed candidates. Ask approval for one exact tag and size before `ollama_manage{action:"pull"}`; discovery never grants installation permission.

`fastest` without evidence is only a capability filter and can pick a tiny model for code repair. For quality-sensitive work preview with `minConfidence:"medium"`; it needs both a task policy from correctness evaluations (`freellama policy from-eval`) and a local `bench-all` report. Never manufacture a policy from throughput. See [Model selection](https://github.com/bgauryy/FreeLlama/blob/main/docs/MODEL_SELECTION.md).

### F — diagnose a failure

Run `doctor`, `doctor{view:"status"}` and `models{view:"resident"}`, then match the symptom. `doctor` reports `status:"degraded"` with `problems` when serve is unreachable or its readiness check names what it waits for (`ollama`, `catalog`, `cpu_backend`). A compacted result's `sections` name what its `response_file` holds; `read_response` returns one section or page.

| Symptom | Cause and action |
|---|---|
| 429 `admission_queue_full` or 503 `admission_timeout` | The selected backend's pool is saturated; retry after `Retry-After` or lower fan-out. Raise only that pool, after reading Ollama's parallel/KV limits ([monitoring](https://github.com/bgauryy/FreeLlama/blob/main/docs/MONITORING.md#adaptive-concurrency)). Never retry a 503 while holding admission. |
| Low-confidence refusal | Add policy plus functional evidence, lower the floor, or use `fastest` and say the evidence is low. |
| Slow but successful | Inspect `execution.observation`, not the assignment: partial GPU offload is silent; free memory or lower context. |
| Intermittent HTTP 500 | Check co-resident memory first; FreeLlama already retries 500/502 and connection errors with backoff. 504s, timeouts and post-send disconnects are final. |
| 404 on a control route | A `proxy` is answering; run `serve`. |
| Research root refusal | Keep `workspacePath` inside an allowlisted root. |
| CLI and server disagree | Restart or align versions before trusting CLI-specific flags. |

## Keep local work fast and light

- One model, one context: send the same `contextTokens` to a model and warm at that context; a different `num_ctx` reloads the runner and drops its prompt cache.
- Stable prefix first: system prompt, tool list and documents before the variable part, and append rather than edit. Warm a long prefix once (`ollama_manage{action:"warm",model,systemPrompt}`) or keep follow-ups in a `scope`; read `metrics.cached_prompt_tokens` to confirm reuse.
- Spend output deliberately: keep the `think:false` defaults outside coding, set `num_predict` from the output contract, and use `stopOnRepetition` for models known to loop.
- Free memory you own: when a burst ends, `ollama_manage{action:"unload",model}` the model you warmed; never unload another client's model unless the user names it, and never stack two large models without the memory arithmetic. A load that does not fit waits, then is refused; do not retry it in a loop, free memory or pick a smaller model.
- Keep your own context small: use the inline answer and verdict, and fetch only the `response_file` section you need with `read_response`.
- Progress: send a `progressToken` and a waiting task or warm reports `waiting for memory`, `loading MODEL (~N s)`, `running`, repeated with the elapsed time every 10 s while a phase lasts; batches also report `k/N items done`. A long wait is the machine protecting itself, not a hang.
- One task reads all its `files` together: a glob gives one answer. For one answer per file, send `tasks:[{id, independent:true, task:{files:[path]}}]`.
- Big jobs are yours to chunk; FreeLlama never splits input, because where to cut changes the answer. Split a long document into pieces that fit the model's window (the receipt's `execution.context_sizing` shows the estimate and `bytes_per_token_milli`; a prompt past the 32k automatic cap is refused fast, so pass `contextTokens` or split; with `contextTokens`, text over 8 bytes per token of that window is refused before any load; both refusals are `context_window_exceeded`). Send the pieces as one `run_task{tasks:[...]}` batch (up to 64 items), or as deferred jobs for long runs, then merge the answers yourself. An embedding item longer than about 3 bytes × the model's context is unsafe: one over 8 bytes per token of the window is refused before any load (`embedding_item_exceeds_context`, with the item indices); chunk it and embed the chunks.

## Routine agent work that local models handle well (measured)

For a model per job, run `models {view:"catalog"}` (CLI: `catalog`); each job has a recipe at `freellama://docs/cookbooks/<job>`.

- Batch with shared fields: `run_task {task, model, questions or systemPrompt, tasks:[{id, independent:true, task:{state or prompt}}]}` sends what the items share once; each item carries only its own input.
- Name inputs by path or glob: `run_task {files:["src/**/*.rs"]}` reads them in the server, so a log, a screenshot or a source file is never pasted into your own output (a base64 screenshot alone is tens of thousands of tokens).
- Locate code for an issue or question: `run_task {task:"embedding", files:[<source files>], nearest:{query:"task: search result | query: <issue>", top:5, documentPrefix:"title: none | text: "}}` with `embeddinggemma-2` (right file first 9/10 here; `nomic-embed-text` with `search_query: ` / `search_document: ` prefixes scored 6/10 but indexes faster), then read only the 5 files it names. Omit `query` for the most similar pairs (duplicates: 8/12 real issue pairs).
- Triage and routing: `run_task {task:"decision", state, questions}` (or `files:[issue.json]` as the state) with `strands-decider` `choice` and `noul` questions (35/36 real issues, 17/17 routing and policy checks); a batch of up to 64 items answers one line each. Do not use its `score` answers for severity.
- Bulk classification: embeddings with three labeled examples per class (18/20); when the top two classes are within 0.02, ask the decider or keep the item yourself.
- Screens and images: `glm-ocr` with the prompt `Text Recognition:` and `stopOnRepetition:true`; crop to the text first (a full page with one line on it reads as garbage). It reads text only; it does not describe images.
- Put embedding models on the CPU backend (`serve --spawn-cpu-backend --cpu-model <embed tag>`): as fast as the GPU here, and they no longer queue behind OCR or chat.
- When a task waits for memory, the progress message and the 503 name the largest memory users; tell the user what is holding the memory rather than retrying.

## Memory, placement and disk

- Budget model weights, KV cache, other residents and the host reserve from current observations; never co-resident two large models without that arithmetic. `OLLAMA_NUM_PARALLEL` multiplies KV by context; unset `OLLAMA_MAX_LOADED_MODELS` resolves to 3 × GPU count. KV precision (`q8_0`, `q4_0`) is a process-wide quality trade-off: compare on representative tasks first.
- For a large GPU model plus a small CPU helper, the operator runs a second Ollama process with `serve --cpu-upstream ... --cpu-model <exact-tag>` and you require observed `size_vram:0`. Some runners (MLX on Metal) stay fully on the GPU even when assigned to CPU. `executionPreference` only chooses among already-eligible models; explicit `model` and session affinity win, and an unmet preference returns `execution.preference_satisfied:false`. Feedback rules: [CPU and GPU routing](https://github.com/bgauryy/FreeLlama/blob/main/docs/CPU_GPU_ROUTING.md).
- `doctor` advises `LLAMA_ARG_CACHE_RAM` (for example 2048 on 32 GB hosts): each runner otherwise may keep up to 8 GiB of displaced prompt caches in host RAM.
- Apply `OLLAMA_*` changes only with human intent, in the environment of the Ollama service, then restart it ([Production](https://github.com/bgauryy/FreeLlama/blob/main/docs/PRODUCTION.md#configure-ollama-explicitly)).
- Disk: treat old models as candidates, never a retention policy. Delete only an exact tag a human names in the current conversation, with `ollama rm <tag>` or `ollama_delete`; never delete files under `~/.ollama/models`.

## Verify every result

| Verdict | Meaning | Action |
|---|---|---|
| `accept` | measured strong model, lookup-shaped task, 1–5 successful grounded calls | use with citations |
| `verify` | unmeasured/weak model, judgment-shaped task, or outside the measured call envelope | independent frontier-tier check |
| `escalate` | unusable model or zero successful calls | discard the answer |

Accept adaptive placement feedback only when `execution.observation.status:"verified"`; for `keepAlive:"0"`, also inspect the observe-then-unload `execution.lifecycle`. Persist model-specific token calibration and bounded placement feedback; use ephemeral modes only for disposable tests. Use current `doctor` output, installed server behavior, and upstream Ollama docs before changing runtime settings.
