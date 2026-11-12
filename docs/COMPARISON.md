# How FreeLlama compares

FreeLlama is a resource-aware task manager for AI agents that use local Ollama models. It is not an
inference engine: Ollama loads and runs the model, FreeLlama decides whether, where and how each
task runs, and returns a compact result with evidence about how it ran.

Other projects' behavior below comes from their own documentation and repositories, checked on
2026-11-12 unless a row says otherwise. Documentation describes an interface, not proof of
reliability or speed, and moving-branch documentation can run ahead of a release. No other stack
was installed or benchmarked for this page; FreeLlama's own figures are in
[measured results](#measured-results), with their limits.

## Where FreeLlama fits

FreeLlama is built for one situation: an external agent (Claude Code, Codex, Cursor, a script)
offloading bounded, checkable work to local models on a workstation that is also doing other work.
For that, the combination it offers is:

- host-aware admission that treats a cold load differently from a request to a loaded model;
- task-aware model picks (installed, fits now) with tested prompts per workflow;
- compact answers with `check:` lines, plus the full receipt on disk, paged on request;
- evidence of what happened: the model and why, waits, load time, and the placement Ollama reported.

Many of the parts exist elsewhere. Loading, unloading, LRU eviction, compatible APIs, MCP and named
presets are common; the comparison is about the policy, the evidence and the agent contract around
them. If you need an organization-wide gateway, an evaluation platform, a desktop app, multi-host
GPU scheduling or a faster engine, another tool on this page is the better fit, often alongside
FreeLlama rather than instead of it.

## Closest alternatives

| Product | What it already does well | Where FreeLlama differs | Prefer it when |
|---|---|---|---|
| Ollama alone | Concurrent residency, memory-dependent queueing, idle eviction, `keep_alive`, preloading, reported CPU/GPU placement, OpenAI and Anthropic compatibility ([FAQ](https://docs.ollama.com/faq), [OpenAI](https://docs.ollama.com/api/openai-compatibility), [Anthropic](https://docs.ollama.com/api/anthropic-compatibility)) | Adds host admission (reserve, OS pressure, swap, CPU load, heat), fair queues across agents, workflow prompts and checkable receipts. FreeLlama builds on these foundations; it does not replace them | One application calls one explicitly chosen model with little contention |
| llama.cpp server/router | A real multi-model router: automatic loading, a maximum of loaded models, device-memory margins with LRU unloading ([server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)) | File inputs, job conventions, host-resource policy and compact evidence for an external agent; engine caching, batching and kernels belong below FreeLlama | You want direct control of the inference engine |
| llama-swap | Capacity pools, coexistence groups, eviction costs, concurrency limits, persistent activity ([features](https://llama-swap.ai/features), [queues](https://llama-swap.ai/docs/guides/routing/capacity-and-queues)). Its MCP endpoint exposes docs, configuration and source inspection ([MCP](https://llama-swap.ai/docs/guides/api-integration/mcp-endpoint)) | Measures host memory at admission instead of operator-declared requirements, and offers a task-offload tool contract. Cost-aware eviction alone does not distinguish FreeLlama | You need flexible lifecycle control across server commands and engines |
| LocalAI | Backend lifecycle, LRU eviction, concurrency groups, watchdogs, model failover, distributed mode, model-backed routing and an agent/admin surface ([memory](https://localai.io/docs/advanced/vram-management/), [failover](https://localai.io/docs/features/model-failover/index.html), [distributed](https://localai.io/docs/features/distributed-mode/index.print.html)) | Narrower and focused on the calling agent keeping judgment and reading small, checkable results | You want the server itself to supply agents, applications, more modalities or several machines |
| Lemonade | Several hardware backends, simultaneous model lifecycles, protected LRU eviction, rule-based and model-backed routing with traces, an MCP inference gateway ([architecture](https://lemonade-server.ai/docs/dev/getting-started/), [router](https://lemonade-server.ai/docs/dev/router-policy/), [MCP](https://lemonade-server.ai/docs/api/mcp/)) | Ranked file search, bounded research, resumable sweeps and explicit host admission | Hardware diversity and configurable routing policies matter most |
| LM Studio | JIT loading, TTL and auto-eviction, headless mode, memory estimates before loading, MCP tool use through its API ([TTL](https://lmstudio.ai/docs/developer/core/ttl-and-auto-evict), [headless](https://lmstudio.ai/docs/developer/core/headless), [load](https://lmstudio.ai/docs/cli/local-models/load), [MCP](https://lmstudio.ai/docs/developer/core/mcp)) | Coordinated external-agent work and inspectable admission; no desktop app | You want a polished app for exploring and serving models |
| oMLX | An Apple-silicon runtime with continuous batching, RAM/SSD KV-cache tiers, LRU eviction, pinning, TTL and a desktop/admin experience ([project](https://github.com/jundot/omlx)) | Ollama-based coordination and an agent contract across macOS, Linux and Windows. FreeLlama implements no batching or KV tiers | Persistent prefix reuse on a Mac is central |
| Docker Model Runner | OCI model distribution in Docker workflows, OpenAI/Ollama-compatible APIs, llama.cpp, vLLM and Diffusers engines, on-demand load and idle unload ([overview](https://docs.docker.com/ai/model-runner/), [configuration](https://docs.docker.com/ai/model-runner/configuration/)) | An agent-oriented task contract and host coordination | Models ship with an existing Docker application or deployment |

## Adjacent tools: compose rather than replace

These solve overlapping problems at another layer. In most deployments the useful relationship is
composition: an application above FreeLlama, or a gateway or engine beside or below it. A combined
stack needs one clear owner for retries, queues, budgets and residency.

| Tool | Stronger at | How it relates to FreeLlama |
|---|---|---|
| LiteLLM | Organization-wide gateway: routing, retries and fallbacks, keys and budgets, response caching ([routing](https://docs.litellm.ai/docs/routing), [reliability](https://docs.litellm.ai/docs/router/learn/reliability), [caching](https://docs.litellm.ai/docs/proxy/caching)) | Put it in front for cross-provider policy; FreeLlama manages the local host |
| TensorZero | Inference and feedback storage, datasets, evaluation, experimentation ([project](https://github.com/tensorzero/tensorzero)) | Far deeper evaluation than FreeLlama's benchmark-to-policy conversion |
| Olla | Endpoint discovery, failover, health handling, cache-affinity sessions across inference endpoints ([project](https://github.com/thushan/olla)) | Fleet routing; FreeLlama admits loads on one host |
| GPUStack | Scheduling models across workers and GPUs ([scheduler](https://docs.gpustack.ai/2.0/scheduler/)) | FreeLlama has no fleet scheduler |
| Open WebUI, AnythingLLM | Multi-user chat, RAG, permissions, agent flows ([Open WebUI](https://docs.openwebui.com/features/), [AnythingLLM](https://docs.anythingllm.com/agent-flows/overview)) | A natural user interface above `freellama gateway` |
| llm, fabric, aichat | Templates, plugins and SQLite logs (llm); a community pattern library (fabric); role files (aichat) ([llm](https://llm.datasette.io/en/stable/templates.html), [fabric](https://github.com/danielmiessler/Fabric), [aichat](https://github.com/sigoden/aichat/wiki/Role-Guide)) | Terminal clients; FreeLlama adds the host coordination they leave to their server |
| RouteLLM | Trained strong/weak routers for a quality-cost trade-off ([project](https://github.com/lm-sys/RouteLLM)) | FreeLlama routes by eligibility, live fit and supplied evidence, not a trained quality router |
| vLLM, SGLang | Runtime prefix caching, sleep/wake memory control, cache-aware serving ([vLLM](https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/), [SGLang](https://github.com/sgl-project/sglang/blob/main/docs/docs/advanced_features/session_radix_cache.mdx)) | Engine features below FreeLlama's layer; reachable as OpenAI-compatible upstreams by model name |

## What is specific, and what is not

| FreeLlama capability | Assessment | What is true elsewhere |
|---|---|---|
| Admission that treats cold loads and requests to loaded models differently, using OS memory, swap, CPU load and heat | The core of the product | Other runtimes have memory fit and eviction; the extra OS policy is what FreeLlama adds |
| Compact answers plus full, paged receipts | Strong for an agent's context budget | Paging and inspection exist elsewhere; the saving should be measured in the agent's total tokens |
| Files by path, ranked line ranges, reused embeddings | Useful | Retrieval is common; the concrete benefit is no raw vectors and no re-embedding unchanged chunks |
| Resumable sweeps with a stronger re-check and baselines | Useful | Checking only flagged chunks cannot prove completeness; misses in "clear" chunks need measuring |
| CPU helper backend with observed placement | Useful and specific | It is a second, operator-assigned Ollama process, not automatic movement of models between devices |
| Explicit fallback chains (`fallbackModels`) | Useful, deliberately narrow | LiteLLM and LocalAI fail over more broadly; FreeLlama moves on only after a refusal before execution |
| User workflow files that replace built-ins | Expected | fabric, llm, aichat and LM Studio all have user presets |
| Cost-aware eviction and predictive warm-up | Not exclusive | Competitors have cost-aware eviction, TTL, pinning and richer engine caching |
| JSON, tools, vision, audio, decisions | Required breadth | Model capabilities come from the model and Ollama; the wrapper adds no accuracy |
| Compatible protocols and virtual workflow models | Distribution, not differentiation | Several alternatives expose compatible APIs and preset wrappers |
| `check:` lines | Hints for verification | Heuristics and model probabilities are not calibrated guarantees or a security boundary |

## Decisions in detail

Per decision, what other local stacks do (sources checked 2026-11-01) and what FreeLlama does.

| Decision | Other local stacks | FreeLlama |
|---|---|---|
| Can this load fit right now? | Ollama already estimates memory, reuses runners, queues work and evicts models ([scheduler](https://github.com/ollama/ollama/blob/main/server/sched.go), [FAQ](https://docs.ollama.com/faq)). The llama.cpp router unloads by LRU within device-memory margins; llama-swap documents declared capacity pools and cost-based eviction; LocalAI has memory policies with backend-dependent estimates ([docs](https://localai.io/docs/advanced/vram-management/)); LM Studio estimates memory before loading. | Checks each load against live available memory plus a reserve sized to the load, unloading idle runners first. A load that still does not fit waits, then is refused with a reason and a `Retry-After` from measured service time; resident models keep serving. While models are loaded, a background sweep unloads idle ones when the host runs short. |
| Is the machine busy or hot? | Ollama and the llama.cpp router do not gate on host CPU load or heat; llama-swap exports swap and GPU temperature as metrics without acting on them ([metrics](https://github.com/mostlygeek/llama-swap/blob/main/internal/perf/prometheus.go)). | Holds CPU-bound work (all work on a host without a GPU) while the 1-minute load average is at least 1.5 per CPU, and CPU-bound work and new loads under Mac thermal pressure; the receipt names the signal. Another program's CPU load never blocks GPU work. Linux and Windows heat is reported as unknown. |
| What loads before it is asked for? | llama-swap, the llama.cpp router (`load-on-startup`) and LocalAI (`--load-to-memory`) load a fixed list at startup. | A startup list, optionally with a prompt prefix evaluated into the runner's prompt cache, plus the model that usually comes next, loaded only when it fits without unloading anything. Ollama owns the prompt cache itself. |
| Who may spend what? | LiteLLM enforces per-key budgets from its database ([users](https://docs.litellm.ai/docs/proxy/users)); llama-swap and the llama.cpp router accept a static key list. | Named client keys with per-key usage, a daily token budget that excludes prompt-cache reuse, and admin rights for model files, on managed and raw traffic alike. The request that crosses a budget can finish, and usage persistence is best effort: this is not billing or tenant isolation. |
| What if the chosen model cannot run? | LiteLLM retries and falls back across deployments, including on context-window errors ([reliability](https://docs.litellm.ai/docs/router/learn/reliability)); LocalAI fails over between models ([failover](https://localai.io/docs/features/model-failover/index.html)). | An explicit, ordered `fallbackModels` chain (up to 4): the next model runs only after the one before was refused before anything executed (no room by the wait deadline, input above its context window, not installed, wrong capability, backend circuit open). Never after a model started generating, so no work is replayed; every attempt is in the receipt. No automatic failover between engines. |
| Other engines? | LiteLLM, Olla and LocalAI route across engines and fail over between them. | OpenAI-compatible engines (LM Studio, llama.cpp, vLLM, a cloud API) behind the same `/v1` endpoint, routed by model name. Managed tasks, residency and the memory budget stay Ollama-focused. |
| Who goes first? | LiteLLM returns 429 once a deployment's parallel limit is reached, with an optional priority queue ([routing](https://docs.litellm.ai/docs/routing)); llama-swap has concurrency limits and queues. | Weighted fair admission per backend (interactive 3 : normal 2 : background 1) with starvation prevention and bounded waits; separate GPU and CPU pools. |
| Which model? | Ollama runs the tag it is given. LiteLLM picks among deployments by load, latency or cost; Lemonade and LocalAI offer rule- and model-backed routing; RouteLLM trains a strong/weak router. | Picks an installed model by capabilities, context window, memory estimate and residency; a model that keeps looping on a task kind ranks last for it. Routing confidence grades the evidence, not the answer. |
| Did it loop? | Ollama aborts some loops with a "token repeat limit" error. | Streams, stops once the output repeats and keeps the text (`stopOnRepetition`); `output_check` on every task. |
| Loading or waiting? | Ollama streams generation and reports final timing; admission phases belong to the coordinator. | MCP progress notifications: `queued`, `waiting for memory: need 9.1 GB, 6.4 GB available`, `loading MODEL (~N s)`, `running`. `GET /ready` answers 503 naming what is missing. |
| What reaches the agent's context? | Ollama returns its native response; presentation depends on the client. | A compact result plus a `response_file` holding thinking, vectors, research observations and receipts, read a section or page at a time with `read_response`. |
| Where did it run? | Ollama exposes running-model memory and placement in `/api/ps`. | A receipt with the chosen model and why, queue and resource waits, and the placement Ollama reported in `/api/ps`, so a silent CPU fallback is visible. |

## Task presets, shell use and protocols

The agent commands and gateway ([reference](AGENT_CLI.md)) compete with tools that wrap a model in a
named preset and serve it to other clients. Their behavior below is taken from their docs, checked
on 2026-11-10.

| Question | Other tools | FreeLlama |
|---|---|---|
| Named task presets? | fabric has patterns, and user patterns override built-ins. aichat has roles: Markdown files with model and sampling front matter ([roles](https://github.com/sigoden/aichat/wiki/Role-Guide)). llm has YAML templates with system, options, schema and tools ([templates](https://llm.datasette.io/en/stable/templates.html)). Open WebUI Workspace Models wrap a base model. LM Studio has config presets ([presets](https://lmstudio.ai/docs/app/presets)). Ollama has Modelfiles ([Modelfile](https://docs.ollama.com/modelfile)). | 15 built-in workflows, each a step-by-step system prompt plus the sampling values, think setting and embedding prefixes each model family documents ([local prompts](PROMPTS.md)). Your own workflows are Markdown files in `~/.octocode/freellama/workflows/`; one named like a built-in replaces it, a bad file is skipped with the reason, and the receipt names the file and its digest ([your own workflows](PROMPTS.md#your-own-workflows)). |
| Which model runs a preset? | Fixed in the preset: fabric `FABRIC_MODEL_PATTERN_NAME`, a role's or template's model, a Modelfile's `FROM`. LiteLLM routes by request complexity. LM Studio estimates memory on request (`lms load --estimate-only`). | The job's tested pick that is installed **and fits in memory now**, the strongest one first for `capable` workflows. Live, with 6.5 GB free: `review-code` moved from `bonsai-2:9b` (10.3 GB) to `lfm3`, and `--ask` ran on `strands-decider` (1.2 GB) instead of a 14.5 GB model. |
| Presets served as models? | fabric `--serveOllama` lists patterns as Ollama models. Open WebUI serves custom models over its OpenAI-style API. A Modelfile becomes a real tag. | `freellama/<workflow>`, user workflows included, on the Ollama, OpenAI Chat, OpenAI Responses and Anthropic Messages APIs from one listener. |
| Which protocols? | aichat `--serve` speaks OpenAI. llama-swap speaks OpenAI and Anthropic. Ollama speaks its own API, OpenAI and Anthropic. | All four. Streamed requests are rewritten and streamed by Ollama itself, so tokens and tool calls stream in every format. Some behavior is inherited from Ollama's compatibility layers. |
| Built for an agent's shell? | llm: fragments, `--schema`, `-x`, a SQLite log. aichat: `-f` for files, directories and URLs, plus `--dry-run`. `ollama run` waits for EOF on any stdin that is not a terminal. | stdout carries only the answer. `check:` lines name what to verify; exit codes are 0, 1, 2, 75 and 130. `-f` takes paths or globs. `--schema` and `-x` follow llm. `--json` prints the receipt. stdin is read only from a pipe or file, so an agent harness's open socket never hangs it. |
| Memory and admission? | Clients of these tools load whatever the preset names. | The same managed admission as MCP: wait for memory, name what holds it, refuse at the deadline. Streamed gateway requests get `serve`'s raw admission. |

What FreeLlama still lacks next to them:

- presets installed from a URL or a community library (llm loaders, LM Studio Hub, Open WebUI);
- `-f` from URLs or command output (aichat): over HTTP the gateway deliberately reads no files and
  fetches no URLs;
- a run log that can be continued (llm); scopes keep history only while `serve` runs;
- a calibrated quality router or an evaluation platform (RouteLLM, TensorZero).

## Who owns which cache

| Resource | Owner | Boundary |
|---|---|---|
| Loaded model weights | Ollama loads and unloads; FreeLlama decides and advises | Model residency management, not OS virtual memory or tensor paging |
| Prompt (KV) reuse | Ollama's runner; FreeLlama warms prefixes and keeps context settings stable between turns | A warm call does not guarantee a cache hit |
| Embedding reuse | FreeLlama's content-hashed index per model, digest and dimensions | A flat cache, not a vector database or a RAG application |
| Learned facts, usage, residency audit | FreeLlama's SQLite file | Shared storage is not a lease: run one FreeLlama authority per host topology |
| Scope history | FreeLlama, in process | Lost on restart |
| Deferred job receipts | In process by default; optional durable journal | Bounded retention; no inference replay |
| Sweep progress | FreeLlama's manifests and JSONL | Resume skips recorded chunks; quality still depends on triage and verification |
| Full response caching | Not provided | Embedding reuse and prefix warming are not response caching |
| Persistent engine KV tiers | Not provided | Runtime stacks such as oMLX or vLLM handle this below FreeLlama |

## Limits

- FreeLlama does not change Ollama's decode speed, batching, kernels or KV cache.
- Admission holds new work; it does not throttle an inference already running, another program can
  allocate memory after admission, and direct calls to Ollama bypass it. The model memory budget is
  neither an OS quota nor a guarantee against every out-of-memory condition.
- CPU work runs only on a second, CPU-only Ollama process (`serve --spawn-cpu-backend` starts and
  stops one for you); a preference cannot move an arbitrary model to the CPU.
- The fallback chain is explicit and Ollama-local; there is no automatic failover between engines.
- Client keys are for a single operator or trusted team: no enterprise identity, tenant isolation,
  audited billing or cluster control plane.
- `check:` lines and decision probabilities are hints. Only strands-decider publishes a calibration
  that FreeLlama cites; other deciders' confidence is treated as a ranking.

## Measured results

The cost of `freellama run` over a direct `ollama` call on identical work is about 0.4 s per call:
0.07 s vs 0.45 s, with the same model, prompt and 2 output tokens, on 2026-11-10. Its defaults can
win that back. Without flags, a direct call let `lfm3` think for 61 tokens to answer "OK" (0.92 s);
the managed call answered in 2 tokens (0.47 s).

Measured on one Apple M2 Pro (32 GB) unless noted. These are single-machine measurements of
specific workloads, mostly from older FreeLlama versions, not a general speed claim and not a
comparison with the alternatives above. The gains come from not loading, waiting or generating what
the task does not need:

| Workload | Without | With FreeLlama | Does not establish |
|---|---|---|---|
| OCR of 12 report strips, `glm-ocr`, 1,500-token cap | 12,948 output tokens, 126 s | `stopOnRepetition`: 6,452 tokens, 69 to 103 s, every label still read | Higher OCR accuracy, or wins on structured output |
| Resident GPU tasks during a compiler build | 64 to 120 s waits (host-wide CPU-load hold, 0.5.0) | No wait: the CPU-load hold applies only to CPU-assigned work | |
| `Retry-After` for a queued burst | 76 s quoted for a 32 s wait (0.5.0) | 32 s | |
| A GPU model and a CPU model side by side (M4 Pro, 48 GB) | 38.0 s median, one Ollama process | 28.2 s median with a second CPU-only Ollama process (1.35x; one of three runs was slower) | A general speed-up, or the same on discrete GPUs |
| Cost of a managed task over raw Ollama, resident embedding, n=20 (0.8.0, 2026-10-31) | raw passthrough: 2.7–2.9 ms median | 4.0 ms median, 5–7 ms p90, with routing, admission and a receipt | CLI start-up cost or cold loads |
| CPU placement through a second CPU-only Ollama (0.8.0, 2026-10-31) | — | `nomic-embed-text:v1.5` reported `processor: "cpu"`, `size_vram: 0`, verified from `/api/ps`; 265 ms load | |
| `gpt-oss:20b` cold load while the host had 12.7 GB available (0.8.0, 2026-10-31) | 0.7.0 admitted it with a memory warning on macOS; a run like this froze the machine | Held (`held_insufficient_capacity`: needs 18.0 GB with the reserve), then refused at the caller's deadline with `Retry-After`; the model never loaded | That every out-of-memory or swap condition is prevented |

A fair comparison with the alternatives needs the same machine, models, settings and workloads,
repeated runs, and published raw results; that benchmark has not been run.
