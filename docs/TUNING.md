# Make local models faster and lighter


## Make local models faster and lighter

Decode speed is fixed by the model and hardware. A task's real cost is mostly around decoding: cold
loads, reloads, re-evaluating a prompt the runner already saw, unneeded thinking and output, and
running out of memory. Best payoff first:

1. **Avoid reloads.** Ollama reuses a loaded runner only when `num_ctx`, `num_batch`, `num_gpu`,
   `num_thread`, `use_mmap` and the context-shift setting match ([scheduler](https://github.com/ollama/ollama/blob/main/server/sched.go)).
   Managed chat sends `truncate:false`, and by default `shift:false`, so an overflow is refused
   instead of silently dropping context; raw clients that keep Ollama's default shift therefore
   reload a runner FreeLlama loaded, and the reverse. Set `context_shift = "ollama_default"` in the
   runtime file to omit `shift` so both share one runner (`truncate:false` is still sent; a long
   generation may then drop its earliest tokens), or give raw and managed traffic separate models
   or backends when both are busy. `freellama config` shows the active value and its source.
   FreeLlama sizes `num_ctx` in fixed buckets (2k–32k), reuses a loaded runner's larger context, and
   leaves thread selection to Ollama unless the operator or caller explicitly overrides it. Keep one model's callers on one
   `contextTokens`, warm at that context, and compare `models`' `context_default` and
   `resident_context`.
2. **Keep prompts prefix-stable.** Ollama's backends own prompt caching; its llama.cpp backend
   reuses matching prompt prefixes. Cache behavior varies by engine and model. Put the stable part first (system prompt, tool list, documents) and
   append, never edit. Warm a long prefix once with `ollama_manage{action:"warm",model,systemPrompt}` (or
   `[preload]` `messages`) and use `scope` for follow-ups, which reuse the whole history. Receipts
   and the usage ledger report `metrics.cached_prompt_tokens` and `uncached_prompt_tokens` from
   Ollama's `prompt_eval_cached_count` when reported; missing counters stay unknown. Context changes
   can reload a runner and lose its prefix cache.
3. **Spend output only where it helps.** Completion, tools, browser and vision tasks send
   `think:false` (Ollama otherwise enables thinking on capable models); coding, code-repair and
   long-context tasks send `think:"medium"` to thinking models. `stopOnRepetition` cut `glm-ocr` on 12
   report strips from 12,948 to 6,452 output tokens with every label still read.
4. **Unload instead of swapping.** A load that does not fit waits instead of swapping, idle models are
   unloaded under memory pressure, and `unload` frees a big model before a switch without ever
   loading it.
5. **Shrink memory per runner.** Quantize the KV cache, keep `OLLAMA_NUM_PARALLEL=1` on a laptop
   (every slot multiplies KV memory by the context), and bound llama-server's hidden prompt cache
   with `LLAMA_ARG_CACHE_RAM`; see [Tune Ollama](#tune-ollama-for-local-models).
6. **Keep the agent's context small.** See [small agent context](FEATURES.md#small-agent-context).

Measured here (Apple M2 Pro, 32 GB, Ollama 0.40.13, FreeLlama 0.8.0, 2026-10-31, light load only):
`serve` prepared in 69–143 ms; a cold `nomic-embed-text:v1.5` load took 0.5 s and a resident embedding
13–26 ms in Ollama, plus 4 ms (median) for FreeLlama; `unload` took 5–56 ms; an MCP embedding result
was 2.6 KB inline with the vectors in its `response_file`.

### Tune Ollama for local models

FreeLlama never changes Ollama's settings. These matter most, per Ollama's docs and measured reports:

| Setting | What it does | Suggested start |
|---|---|---|
| `OLLAMA_NUM_PARALLEL` | Decoding slots per model, each with its own prompt cache; RAM grows with slots times context ([Ollama FAQ](https://github.com/ollama/ollama/blob/main/docs/faq.mdx)). Small gain on Apple silicon; some engines run one request at a time ([MLX engine](https://github.com/ollama/ollama/issues/17280)) | `1`, raised only after measuring. FreeLlama's budget follows it |
| `OLLAMA_FLASH_ATTENTION` + `OLLAMA_KV_CACHE_TYPE=q8_0` | A quantized KV cache roughly halves its memory and needs flash attention ([Ollama FAQ](https://github.com/ollama/ollama/blob/main/docs/faq.mdx)); with `OLLAMA_FLASH_ATTENTION` unset, Ollama enables it automatically on supported backends. See [KV cache and flash attention](#kv-cache-and-flash-attention) | On, when the model's quality holds |
| Context length | Ollama defaults to 4k below 24 GiB of VRAM and recommends at least 64k for agent and coding work ([context length](https://github.com/ollama/ollama/blob/main/docs/context-length.mdx)) | Size per task. FreeLlama keeps Ollama's default when it covers the prompt, otherwise sizes text tasks to the prompt; `contextTokens` overrides |
| `OLLAMA_MAX_LOADED_MODELS` | How many runners stay resident. Unset and `0` mean 3 per GPU, then Ollama unloads by count | More than the model memory budget can hold, so the budget decides by size and reload cost. `1` reloads on every switch |
| `keep_alive` | How long a runner stays loaded after use | Leave it out. FreeLlama picks a finite time from measured reuse |
| `LLAMA_ARG_CACHE_RAM` | Host RAM llama-server may use for displaced prompt caches (`--cache-ram`); default 8192 MiB per runner ([llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)), counted by no footprint estimate. Set it on the Ollama server, which passes its environment to llama-server; `doctor` checks it | `2048` on 32 GB or less; `0` disables it at the cost of re-evaluating prefixes |

#### KV cache and flash attention

FreeLlama sizes each backend's KV cache from that Ollama process's settings. A model whose metadata
does not describe its cache (hybrid, recurrent or sliding-window layers) is assumed to need a quarter
of its file size per 16K tokens of context, for each parallel slot, until its first load is
measured (`source: model_file_plus_assumed_kv_plus_graph` in the receipt):

| `OLLAMA_FLASH_ATTENTION` | State | KV share of f16 with `q8_0` / `q4_0` |
|---|---|---|
| unset | `auto`: Ollama enables flash attention where the backend supports it (Metal, CUDA) | 50% / 25% |
| `1`, `true` | `on` | 50% / 25% |
| `0`, `false` | `off`: Ollama ignores `OLLAMA_KV_CACHE_TYPE` | 100% |

`auto` is sized like `on`. A model or backend without flash attention falls back to an f16 cache, so
its first load can be larger than the estimate; the measured footprint, keyed by
`OLLAMA_NUM_PARALLEL` and the KV cache share, then sizes later loads. `status` reports the result
under `ollama.config.effective` (`cpu_config` for a CPU backend) and `config` under
`ollama.effective`: `flash_attention`, `kv_cache_type` and `kv_cache_percent_of_f16`.

Ollama's own fit check still predicts an f16 cache, and its scheduler evicts other runners or
offloads layers when that prediction passes 80% of free memory. FreeLlama reserves the quantized
estimate (or a measurement) and keeps Ollama's f16 figure beside it as
`memory_reservation.ollama_f16_preflight_bytes`. When the quantized estimate fits within 80% of the
available memory FreeLlama admitted against but the f16 figure does not, the receipt adds
`ollama_may_evict_or_offload: true` and `ollama_preflight_reason`. This applies to CPU and
unified-memory backends; on a discrete GPU Ollama compares against VRAM, which FreeLlama does not
attribute to a backend.
