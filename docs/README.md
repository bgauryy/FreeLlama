# FreeLlama documentation

FreeLlama helps agents offload bounded work to local models through Ollama. The calling agent owns
the task and verifies the answer; FreeLlama manages model selection and resources around execution.
Start with the [repository MCP setup](https://github.com/bgauryy/FreeLlama/blob/main/README.md#mcp-server) to run one managed task.

## Choose a guide

| Goal | Guide |
|---|---|
| Connect an agent and choose a tool | [MCP server](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md) |
| Follow the agent's operating workflow | [FreeLlama skill](https://github.com/bgauryy/FreeLlama/blob/main/skills/freellama/SKILL.md) |
| Look up commands and task controls | [CLI reference](CLI.md) |
| Understand task and resource ownership | [Architecture](ARCHITECTURE.md) |
| Choose a model and interpret its metadata | [Model selection](MODEL_SELECTION.md), [model metadata](MODEL_SELECTION.md#model-metadata) |
| Configure CPU helpers alongside GPU work | [CPU/GPU routing](CPU_GPU_ROUTING.md) |
| Inspect queues, memory, usage, and live settings | [Monitoring](MONITORING.md) |
| Retain history, affinity, or residency | [Scopes and warming](SCOPES_AND_WARMING.md) |
| Understand raw Ollama compatibility | [Ollama sidecar](ARCHITECTURE.md#ollama-sidecar) |
| Evaluate context offload and its cost | [Token economics](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md#token-economics) |
| Deploy the management layer | [Production runbook](PRODUCTION.md) |
| Run development and release checks | [Testing](TESTING.md), [release procedure](https://github.com/bgauryy/FreeLlama/blob/main/RELEASE.md) |
| Embed the routing core | [Rust core](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/README.md) |
| Use the read-only status page | `/_freellama/ui` on a running `serve` (see [Monitoring](MONITORING.md)) |

## Evidence and design

| Goal | Source |
|---|---|
| Review dated findings and project comparisons | [Findings report](dev/FINDINGS_AND_POSITIONING.md) |
| Measure inference, device activity, and thermal behavior | [Local performance testing](dev/LOCAL_PERFORMANCE_TESTING.md) |
| Separate management controls from inference tuning | [Ollama optimization boundary](dev/OLLAMA_SYSTEM_OPTIMIZATION.md) |
| Understand backend selection decisions | [Resource-routing ADR](dev/ADR_RESOURCE_AWARE_BACKEND_ROUTING.md) |

Historical measurements describe one model, machine, and workload. Inspect current state and
repeat the relevant checks before using them to select models or set resource limits.
