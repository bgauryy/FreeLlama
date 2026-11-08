# FreeLlama documentation

FreeLlama helps agents offload bounded work to local models through Ollama. The calling agent owns
the task and verifies the answer; FreeLlama manages model selection and resources around execution.
Start with the [repository README](https://github.com/bgauryy/FreeLlama/blob/main/README.md#install) to install and run one managed task.

## Choose a guide

| Goal | Guide |
|---|---|
| Connect an agent and choose a tool | [MCP server](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md) |
| Follow the agent's operating workflow | [FreeLlama skill](https://github.com/bgauryy/FreeLlama/blob/main/skills/freellama/SKILL.md) |
| Look up commands and task controls | [CLI reference](CLI.md) |
| Choose a model for a job | `models {view:"catalog"}`, `freellama catalog`, or the [local model catalog](LOCAL_MODELS.md) |
| Follow a recipe for one job | [Cookbooks](cookbooks/README.md) |
| Teach an agent the Octocode CLI in under 30 lines | [Local tools](octocode/LOCAL.md), [GitHub tools](octocode/GITHUB.md) |
| Understand task and resource ownership | [Architecture](ARCHITECTURE.md) |
| See every feature, routing and admission in depth, and why FreeLlama exists | [Features](FEATURES.md) |
| Compare FreeLlama with Ollama, llama.cpp, llama-swap and LocalAI | [Comparison](COMPARISON.md) |
| See supported model forms and dated measured results | [Model forms and results](MODEL_FORMS_AND_RESULTS.md) |
| Reduce reloads, prompt cost and memory; tune Ollama settings | [Tuning](TUNING.md) |
| Build from source; read operating notes (response store, port 11435, certificates) | [Build and notes](BUILD_AND_NOTES.md) |
| Choose a model and interpret its metadata | [Model selection](MODEL_SELECTION.md), [model metadata](MODEL_SELECTION.md#model-metadata) |
| Inspect quantization, placement, or neuron activations | [Model introspection](MODEL_INTROSPECTION.md) |
| Configure CPU helpers alongside GPU work | [CPU/GPU routing](CPU_GPU_ROUTING.md) |
| Inspect queues, memory, usage, and live settings | [Monitoring](MONITORING.md) |
| Retain history, affinity, or residency | [Scopes and warming](SCOPES_AND_WARMING.md) |
| Understand raw Ollama compatibility | [Ollama sidecar](ARCHITECTURE.md#ollama-sidecar) |
| Evaluate context offload and its cost | [Token economics](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md#token-economics) |
| Deploy the management layer | [Production runbook](PRODUCTION.md) |
| Run development and release checks | [Testing](TESTING.md), [release packages](TESTING.md#verify-release-packages) |
| Embed the routing core | [Rust core](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/README.md) |
| Use the read-only status page | `/_freellama/ui` on a running `serve` (see [Monitoring](MONITORING.md)) |
| Configure durable receipts, idempotency and bounded job retention | [Deferred jobs and recovery](DEFERRED_JOBS.md) |

Historical measurements describe one model, machine, and workload. Inspect current state and
repeat the relevant checks before using them to select models or set resource limits.
