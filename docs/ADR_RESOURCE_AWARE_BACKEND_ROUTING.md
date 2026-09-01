# ADR: Resource-aware backend routing

Status: Accepted and implemented

## Context

FreeLlama can isolate explicitly assigned helper models in a second Ollama process. Live trials
proved that CPU and GPU requests can overlap, but exact operator assignment alone was too rigid for
agents, and one global admission pool can let GPU work starve a CPU helper. Unconstrained
automatic migration is unsafe because capability, quality, session, and memory constraints
must continue to win.

## Decision

Keep backend eligibility operator-owned and add three bounded controls:

1. Callers may send `auto`, `prefer_cpu`, or `prefer_gpu`; this is a fallback-capable hint.
2. GPU and CPU backends own independent weighted admission pools and transition locks.
3. `auto` can use task-specific normalized warm latency after three samples exist on both backends
   and one backend has more than a 10% advantage.

Explicit models and session affinity override the hint. Quality routing never follows latency.
Generation normalizes by output tokens; embeddings normalize by input tokens. Cold-load duration
does not enter feedback. Raw Ollama passthrough remains byte-transparent to the primary upstream.
Verified feedback is stored in a bounded, versioned snapshot and replaced atomically. A corrupt or
unsupported snapshot fails startup instead of silently changing learned routing behavior.

The contract is hardware-neutral: the operator supplies loopback endpoints and exact CPU-eligible
tags, while FreeLlama discovers host capacity and observes Ollama placement. The measured 48 GB Mac
result is validation evidence, not a routing constant. Device-visibility variables remain
Ollama-process configuration because NVIDIA, ROCm, Vulkan, Metal, and CPU-only hosts differ.

## Alternatives

| Alternative | Decision | Reason |
|---|---|---|
| Exact assignment only | Rejected | Safe but too rigid for agents and blind to runtime capacity |
| Let agents supply an upstream or `num_gpu` | Rejected | Bypasses operator eligibility and compatibility boundaries |
| Rewrite every raw Ollama request | Rejected | Breaks passthrough semantics and surprises existing clients |
| Persist every raw runtime event | Rejected | Unbounded traces add privacy and storage risk; persist only aggregate prompt-free feedback |
| Live memory/thermal controller | Deferred | No stable sensor and held-out workload yet justify it |

## Verification contract

- Preview returns placement, upstream, preference satisfaction, reason, and admission capacity.
- Two samples per backend do not change `auto`; the third makes the comparison decision-ready.
- Explicit model and session pins do not move.
- A one-unit GPU pool and one-unit CPU pool admit one task on each concurrently.
- CPU-managed requests contain `options.num_gpu: 0`; raw `/api/*` bodies are unchanged.
- Health exposes both pools and per-task feedback counts.
- Restart reloads the same bounded feedback counts; an unsupported schema refuses startup.

Rollback is configuration-only for callers: omit the preference or use `auto`. Operators can remove
the CPU backend to restore a single-primary layout. Reverting independent pools requires a code
rollback and is not recommended because it reintroduces cross-backend starvation.
