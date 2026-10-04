# FreeLlama documentation

FreeLlama helps agents offload bounded work to local models through Ollama. The calling agent owns
the task and verifies the answer; FreeLlama manages model selection and resources around execution.
Start with the [repository quick start](../README.md#quick-start) to run one managed task.

## Choose a guide

| Goal | Guide |
|---|---|
| Connect an agent and choose a tool | [MCP server](../packages/mcp/README.md) |
| Follow the agent's operating workflow | [FreeLlama skill](../skills/freellama/README.md) |
| Look up commands and task controls | [CLI reference](CLI.md) |
| Understand task and resource ownership | [Architecture](ARCHITECTURE.md) |
| Choose a model from measured evidence | [Model selection](MODEL_SELECTION.md) |
| Interpret capabilities and public model metadata | [Model metadata](MODEL_METADATA.md) |
| Configure CPU helpers alongside GPU work | [CPU/GPU routing](CPU_GPU_ROUTING.md) |
| Inspect queues, memory, usage, and live settings | [Monitoring](MONITORING.md) |
| Retain history, affinity, or residency | [Scopes and warming](SCOPES_AND_WARMING.md) |
| Understand raw Ollama compatibility | [Sidecar boundary](OLLAMA_SIDECAR.md) |
| Evaluate context offload and its cost | [Token economics](ECONOMICS.md) |
| Deploy the management layer | [Production runbook](PRODUCTION.md) |
| Run development and release checks | [Testing](TESTING.md), [release procedure](../RELEASE.md) |
| Embed the routing core | [Rust core](../packages/rust-core/README.md) |
| Use the read-only dashboard | [Runtime view](../packages/view/README.md) |

## Evidence and design

| Goal | Source |
|---|---|
| Describe the product and its limits | [Product positioning](PRODUCT_POSITIONING.md) |
| Review dated findings and project comparisons | [Findings report](dev/FINDINGS_AND_POSITIONING.md) |
| Measure inference, device activity, and thermal behavior | [Local performance testing](dev/LOCAL_PERFORMANCE_TESTING.md) |
| Separate management controls from inference tuning | [Ollama optimization boundary](dev/OLLAMA_SYSTEM_OPTIMIZATION.md) |
| Understand backend selection decisions | [Resource-routing ADR](dev/ADR_RESOURCE_AWARE_BACKEND_ROUTING.md) |
| Choose a model, adapter, or hardware benchmark | [Benchmark index](../benchmark/README.md) |
| Inspect research adapter contracts and caveats | [Adapter reference](../benchmark/local/docs/07-adapter-contracts.md) |

Historical measurements describe one model, machine, and workload. Inspect current state and
repeat the relevant checks before using them to select models or set resource limits.
