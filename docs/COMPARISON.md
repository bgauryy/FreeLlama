# How FreeLlama compares

Moved from the root README.

## How it compares

FreeLlama is not an inference engine: Ollama loads and runs the model, FreeLlama decides whether,
where and how each task runs. Other projects' behavior is taken from their docs and source, checked
on 2026-11-01. Moving-branch features may differ from installed releases; see the
[Ollama resource audit](OLLAMA_RESOURCE_AUDIT_2026-11-01.md).

| Decision | Other local stacks | FreeLlama |
|---|---|---|
| Can this load fit right now? | Ollama already estimates memory, reuses runners, queues work and evicts models ([scheduler](https://github.com/ollama/ollama/blob/main/server/sched.go), [FAQ](https://docs.ollama.com/faq)). llama.cpp has model loading and eviction ([server docs](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)); llama-swap documents declared capacity pools and cost-based eviction ([config](https://github.com/mostlygeek/llama-swap/blob/main/docs/config.example.yaml)); LocalAI has memory-management policies with backend-dependent estimates ([docs](https://localai.io/docs/advanced/vram-management/)). | Checks each load against live available memory plus a reserve sized to the load, unloading idle runners first. A load that still does not fit waits, then is refused with a reason and a `Retry-After` from measured service time; resident models keep serving. While models are loaded, a background sweep unloads idle ones when the host runs short. |
| Is the machine busy or hot? | Ollama and the llama.cpp router do not gate on host CPU load or heat; llama-swap exports swap and GPU temperature as metrics without acting on them ([metrics](https://github.com/mostlygeek/llama-swap/blob/main/internal/perf/prometheus.go)). | Holds CPU-bound work (all work on a host without a GPU) while the 1-minute load average is at least 1.5 per CPU, and CPU-bound work and new loads under Mac thermal pressure; the receipt names the signal. Another program's CPU load never blocks GPU work. |
| What loads before it is asked for? | llama-swap, the llama.cpp router (`load-on-startup`) and LocalAI (`--load-to-memory`) load a fixed list at startup ([llama-swap](https://github.com/mostlygeek/llama-swap/blob/main/docs/config.example.yaml), [llama.cpp](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md), [LocalAI](https://github.com/mudler/LocalAI/blob/master/docs/content/reference/cli-reference.md)). These examples describe startup configuration, not an exhaustive feature comparison. | A startup list, optionally with a prompt prefix evaluated into the runner's prompt cache, plus the model that usually comes next, loaded only when it fits without unloading anything. |
| Who may spend what? | LiteLLM enforces per-key budgets from its database ([users](https://docs.litellm.ai/docs/proxy/users)); llama-swap and the llama.cpp router accept a static key list. | Named client keys with per-key usage, a daily token budget that excludes prompt-cache reuse, and admin rights for model files, on managed and raw traffic alike. |
| Other engines? | LiteLLM, Olla and LocalAI route across engines and fail over between them ([LocalAI failover](https://github.com/mudler/LocalAI/blob/master/docs/content/features/model-failover.md)). | OpenAI-compatible engines (LM Studio, llama.cpp, vLLM, a cloud API) behind the same `/v1` endpoint, routed by model name. |
| Who goes first? | LiteLLM returns 429 once a deployment's parallel limit is reached, with an optional priority queue ([routing](https://docs.litellm.ai/docs/routing), [scheduler](https://docs.litellm.ai/docs/scheduler)). | Weighted fair admission per backend (interactive 3 : normal 2 : background 1) with starvation prevention and bounded waits; separate GPU and CPU pools. |
| Which model? | Ollama runs the tag it is given. LiteLLM picks among deployments by load, latency or cost; RouteLLM trains a strong/weak router ([RouteLLM](https://github.com/lm-sys/RouteLLM)). | Picks an installed model by capabilities, context window, memory estimate and residency; a model that keeps looping on a task kind ranks last for it. |
| Did it loop? | Ollama aborts some loops with a "token repeat limit" error. | Streams, stops once the output repeats and keeps the text (`stopOnRepetition`); `output_check` on every task. |
| Loading or waiting? | Ollama streams generation and reports final timing; outer admission and host-resource phases belong to the coordinator. | MCP progress notifications (when the client sends a `progressToken`): `queued`, `waiting for memory: need 9.1 GB, 6.4 GB available`, `loading MODEL (~N s)`, `running`; the answer line adds `loaded N s, evicted X`. `GET /ready` answers 503 naming what is missing (`ollama`, `cpu_backend`, `catalog`). |
| What reaches the agent's context? | Ollama returns its native response; presentation and filtering depend on the client. | A compact result (answer, verdict, model, tokens) plus a `response_file` holding thinking, vectors, research observations and receipts, read a section or page at a time with `read_response`. |
| Where did it run? | Ollama exposes running-model memory/placement in `/api/ps` and generation timing in responses. | A receipt with the chosen model and why, queue and resource waits, and the placement Ollama reported in `/api/ps`, so a silent CPU fallback is visible. |

Limits: FreeLlama does not change Ollama's decode speed, does not fail over between engines, and runs
CPU work only on a second, CPU-only Ollama process (`serve --spawn-cpu-backend` starts and stops one for you).

Measured on one Apple M2 Pro (32 GB) unless noted; single-machine measurements of specific
workloads, not a general speed claim. The gains come from not loading, waiting or generating what the
task does not need:

| Workload | Without | With FreeLlama |
|---|---|---|
| OCR of 12 report strips, `glm-ocr`, 1,500-token cap | 12,948 output tokens, 126 s | `stopOnRepetition`: 6,452 tokens, 69 to 103 s, every label still read |
| Resident GPU tasks during a compiler build | 64 to 120 s waits (host-wide CPU-load hold, 0.5.0) | No wait: the CPU-load hold applies only to CPU-assigned work |
| `Retry-After` for a queued burst | 76 s quoted for a 32 s wait (0.5.0) | 32 s |
| A GPU model and a CPU model side by side (M4 Pro, 48 GB) | 38.0 s median, one Ollama process | 28.2 s median with a second CPU-only Ollama process (1.35x; one of three runs was slower) |
| Cost of a managed task over raw Ollama, resident embedding, n=20 (0.8.0, 2026-10-31) | raw passthrough: 2.7–2.9 ms median | 4.0 ms median, 5–7 ms p90, with routing, admission and a receipt (56 ms before the per-task process probe was cached) |
| CPU placement through a second CPU-only Ollama (0.8.0, 2026-10-31) | — | `nomic-embed-text:v1.5` reported `processor: "cpu"`, `size_vram: 0`, verified from `/api/ps`; 265 ms load |
| `gpt-oss:20b` cold load while the host had 12.7 GB available (0.8.0, 2026-10-31) | 0.7.0 admitted it with a memory warning on macOS; a run like this froze the machine | Held (`held_insufficient_capacity`: needs 18.0 GB with the reserve), then refused at the caller's deadline with `Retry-After`; the model never loaded |
