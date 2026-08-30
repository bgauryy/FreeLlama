# Ollama sidecar boundary

We recommend integrating FreeLlama with Ollama as an API-compatible sidecar,
not as an
in-process engine plugin. Ollama documents a stable, backward-compatible HTTP
API but does not document an external runner or scheduler plugin interface.
The sidecar therefore owns cross-cutting request policy while Ollama continues
to own model loading, scheduling, Metal, or MLX execution, and token generation.

## What the sidecar gives us

- One Ollama-compatible endpoint for local clients.
- Byte-stream forwarding for native Ollama streaming responses.
- A receipt header and a place to add request IDs, metrics, budgets, admission
  rules, model aliases, and routing policy.
- A safe default: localhost binding, recursion rejection, and no accidental
  network exposure.
- A Rust boundary that does not require an Ollama fork.

The sidecar does not increase prompt or decode tokens per second. It can improve
end-to-end workload efficiency only when it prevents avoidable model reloads,
rejects work that can cause memory pressure, coordinates model transitions,
or routes a task to a suitable resident model.

The active managed-task implementation routes tasks, prefers eligible resident
models, shares execution permits for resident work, and serializes nonresident
transitions. Native passthrough requests remain under Ollama's scheduler. The
sidecar does not enforce memory-pressure admission. See
[Ollama and FreeLlama optimization](OLLAMA_SYSTEM_OPTIMIZATION.md) for the
shipped-versus-planned audit.

## What stays upstream

| Problem | Owner |
|---|---|
| Metal, MLX, or llama.cpp kernels | Ollama and its engine dependencies |
| Prompt processing or decode speed | Ollama runner and model format |
| Runner loading, eviction, and scheduling | Ollama scheduler |
| Prompt templates and model defaults | Ollama model metadata or a `Modelfile` |
| Authentication, policy, routing receipts, and workload telemetry | FreeLlama sidecar |

Do not add an Ollama fork or generic plugin ABI until the API boundary cannot
solve a measured problem. Submit engine and scheduler improvements to
the component that owns them; use the frozen stock-versus-candidate suite in
this repository to validate those changes.

## Compatibility contract

The implementation in `packages/rust-core/src/proxy.rs` forwards all paths and query strings and
streams both request and response bodies. It removes hop-by-hop transport
headers and adds `x-freellama-proxy: 1` to the response. A live smoke test
verified that `/api/version` returned the same body through direct Ollama and
FreeLlama.

Ollama's API is available at `http://localhost:11434/api`. Ollama documents the
API as backward compatible. Several endpoints use newline-delimited JSON streaming by
default, which is why FreeLlama must not buffer or re-encode responses.

Sources:

- [Ollama API introduction](https://docs.ollama.com/api/introduction)
- [Ollama streaming behavior](https://docs.ollama.com/api/streaming)
- [Ollama usage metrics](https://docs.ollama.com/api/usage)
