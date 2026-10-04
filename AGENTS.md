# Agents

FreeLlama exposes a management layer that lets AI agents offload bounded tasks to local Ollama
models. Keep model routing and resource coordination in the Rust core; the calling agent owns
prompts, task dependencies, judgment, and verification. Ollama and the host execute physical work.

## Repository operating index

| Need | Source of truth |
|---|---|
| Install, run, and find a guide | [README](README.md), [documentation index](docs/README.md) |
| Architecture and ownership | [Architecture](docs/ARCHITECTURE.md) |
| MCP tool choice, request shapes, and agent workflow | [MCP reference](packages/mcp/README.md) |
| Ollama/model operation procedure | [FreeLlama skill](skills/freellama/SKILL.md) |
| CPU/GPU topology and observed placement | [CPU/GPU routing](docs/CPU_GPU_ROUTING.md) |
| Runtime resource limits and status | [Monitoring](docs/MONITORING.md) |
| Generic benchmark execution and scoring | [Harness guide](benchmark/harness/README.md) |
| Local adapter comparison | [Local benchmark](benchmark/local/README.md) |
| Adapter behavior, configuration, and measured caveats | [Adapter contracts](benchmark/local/docs/07-adapter-contracts.md) |
| Development checks | [Testing](docs/TESTING.md), root `package.json` |

## Agent-facing invariants

- Inspect current state. Do not infer installed tags, residency, memory fit, or physical CPU/GPU
  placement from a model name.
- Treat `run_task {preview:true}` as a decision-only request with routing fields. It rejects
  prompts, messages, embedding input, tools, images, and runtime controls. Review the decision,
  then submit a separate execution call; preview does not reserve capacity.
- Model search and recommendation do not authorize a pull. Require explicit approval for one
  exact tag and its reported size. Stop or delete only an explicitly approved exact installed tag.
- CPU eligibility is operator-owned. Preferences cannot move arbitrary models to another backend.
  Configured assignment is intent; require returned observed evidence when placement matters.
- `delegate_research` is bounded read-only lookup. The caller retains decomposition, judgment,
  mutation authority, and final verification. Discard an `escalate` result.
- An empty LSP result is ambiguous. Retry or search instead; a cold zero does not prove a language
  is unsupported. Structural code search requires `pattern` or `rule`, not `keywords` alone.
- Adapter implementations live only under `benchmark/local/`, on the generic scoring infrastructure
  in `benchmark/harness/`. Preserve full audit output; paginate what the model reads.
- Keep comparison suites, fixtures, answer keys, schemas, and graders frozen during measurement.
  Local benchmark judging is a separate non-local post-hoc step; do not co-resident a local judge
  with the candidate model.

## Package manager and checks

Use Yarn for the workspace and Cargo for Rust. Choose the checks for the changed surface:

| Surface | Command |
|---|---|
| TypeScript and MCP contracts | `yarn typecheck` and `yarn test` |
| Rust contracts | `yarn test:rust` |
| Local adapters | `yarn test:agents` |
| Production release gate | `yarn verify:production` |

See the owning guides for prerequisites and live-system checks.
