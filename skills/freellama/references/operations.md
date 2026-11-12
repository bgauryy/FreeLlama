# Operations: health, memory, placement and verification

Load when a call fails, before a large batch, when memory fit or CPU/GPU placement matters, or when grading a result.

## Ownership

The calling agent owns decomposition, prompts and verification. The operator owns endpoints, exact `--cpu-model` assignments, Ollama settings, and pull, stop and delete approval. FreeLlama routes, admits and observes; Ollama and the OS execute. A CPU preference cannot move an arbitrary tag, and one managed call contains one task. [Architecture](https://github.com/bgauryy/FreeLlama/blob/main/docs/ARCHITECTURE.md#responsibility-boundary).

## Check health

1. `doctor` (CLI `freellama doctor`) reports `status:"degraded"` with `problems` when serve is unreachable or its readiness waits on `ollama`, `catalog` or `cpu_backend`. FreeLlama does not install Ollama: if it is missing or down, point the operator to `https://ollama.com/download` or `ollama serve`, then stop. Do not recommend or pull a model.
2. `doctor {view:"status"}` (CLI `freellama status`) shows free memory, what holds it, admission slots per backend, loaded models and active jobs. Zero slots means queueing up to the configured wait, not a reservation.
3. `run_task`, `delegate_research`, installed and resident views need serve. `freellama proxy` only retries raw `/api/*`; a 404 on a control route means a proxy answered, not serve.

## Symptoms

| Symptom | Cause and action |
|---|---|
| Exit 75 / `resource_admission_unavailable`, names the largest memory users | Memory is short. Tell the user what holds it; never stop their processes. Retry once memory frees, or use a smaller installed model with the same capability. Do not loop. |
| 429 `admission_queue_full` or 503 `admission_timeout` | The backend pool is saturated: retry after `Retry-After` or lower fan-out. Raise only that pool, after reading Ollama's parallel and KV limits ([monitoring](https://github.com/bgauryy/FreeLlama/blob/main/docs/MONITORING.md#adaptive-concurrency)). Never retry a 503 while holding admission. |
| Low-confidence refusal (`minConfidence`) | Add a policy plus functional evidence, lower the floor, or use `fastest` and say the evidence is low. |
| Exit 1 "tool call although no tools were sent", or any `check:` warning | A failed or doubtful answer: rerun on a stronger installed pick ([jobs and models](jobs-and-models.md#field-tested-model-behavior)). |
| `check:` "ran on X because the stronger installed pick Y did not fit now" | Memory forced a weaker model. Verify, or rerun when memory frees or with `model`. |
| `job_no_model_fits` | No pick is installed with room. Report free memory and what holds it; pass an installed model with the same capability, or ask approval to pull one exact tag. |
| Empty answer after thinking | The model spent its budget thinking: `--think=false` or raise `num_predict`. |
| Slow but successful | Read `execution.observation`, not the assignment: partial GPU offload is silent. Free memory or lower context. |
| Intermittent HTTP 500 | Check co-resident memory first; FreeLlama already retries 500/502 and connection errors. 504s, timeouts and post-send disconnects are final. |
| Research root refusal | Keep `workspacePath` inside an allowlisted root. |

## Memory, placement and disk

- On unified memory read `doctor.machine.memory_bytes` (`memory_kind:"unified"`); on discrete GPUs read VRAM from `/api/ps`, never from RAM. Budget weights, KV cache, other residents and the host reserve from current observations. `OLLAMA_NUM_PARALLEL` multiplies KV by context; unset `OLLAMA_MAX_LOADED_MODELS` resolves to 3 × GPU count. KV precision (`q8_0`, `q4_0`) is a process-wide quality trade-off.
- `execution.backend` is configuration; `execution.observation.processor` is evidence. A mismatch or partial placement warning invalidates feedback. Accept adaptive placement feedback only when `execution.observation.status:"verified"`.
- A small CPU helper beside a large GPU model needs a second Ollama process (`serve --cpu-upstream … --cpu-model <exact-tag>`) and observed `size_vram:0`. Some runners (MLX on Metal) stay on the GPU even when assigned to CPU. `executionPreference` (`prefer_cpu`, `prefer_gpu`) only chooses among eligible models; explicit `model` and session affinity win; an unmet preference returns `execution.preference_satisfied:false`. [CPU and GPU routing](https://github.com/bgauryy/FreeLlama/blob/main/docs/CPU_GPU_ROUTING.md).
- `doctor` advises `LLAMA_ARG_CACHE_RAM` (for example 2048 on 32 GB hosts). Apply `OLLAMA_*` changes only with human intent, in the Ollama service's environment, then restart it ([production](https://github.com/bgauryy/FreeLlama/blob/main/docs/PRODUCTION.md#configure-ollama-explicitly)).
- Unload only a model you warmed (when its burst ends) or one the user names. Delete only an exact installed tag the user names in this conversation (`ollama_delete` or `ollama rm`); never delete files under `~/.ollama/models`. Old models are candidates, not a retention policy.
- `bench-all` talks to Ollama directly, past the memory gate: run it on a quiet machine with `--include` for the tags you mean, and `--output` outside the repo's evidence file unless you intend to replace it. It covers chat, tools, vision, embedding and decision models; speech-only models get no case. A model evicted by another client after its answer is graded normally with a note.

## Grade the result

| Verdict | When | Action |
|---|---|---|
| `accept` | Measured strong model, lookup-shaped task, 1–5 successful grounded calls, no `check:` warning | Use, with citations |
| `verify` | Unmeasured or small model, judgment-shaped task, a downgraded pick, an `[unsure]` decision, or any `check:` line | Check it yourself before relying on it |
| `escalate` | Unusable model, zero successful calls, a tool call nobody asked for, empty answer | Discard and do the work yourself |

For a batch, check 3–5 items (more where a mistake costs more) before trusting the rest. Treat every measured number as one-machine evidence.
