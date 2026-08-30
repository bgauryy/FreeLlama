---
name: freellama
description: "Use when orchestrating local Ollama models through the FreeLlama MCP server — deciding what to offload and to which tier, routing with evidence, checking memory and residency before a large call, choosing or installing a model, or diagnosing Ollama/FreeLlama failures (HTTP 500s, timeouts, version mismatch, proxy vs serve)."
---

# FreeLlama

How any orchestrating agent should drive the FreeLlama MCP server: push work down to free local
models without letting a weak model quietly get it wrong.

## Discover this machine before trusting any number here

Model names, sizes, and scores in these references come from **one** setup. Yours will differ.
Three calls give you the real picture:

```
doctor                      memory, chip, and the 9 OLLAMA_* settings with effective defaults
models {view:"installed"}   what you have: capabilities, advertised context, policy rank
models {view:"resident"}    what is loaded now, and its GPU/CPU split
```

If nothing installed fits: `recommend` (curated plan) or `search_models` (search the library →
inspect a family's tags → memory fit against *your* machine).

## Rules that hold on any machine

- **Accuracy collapses below roughly 12B** for grounded code research. Measured here: 7B 2/8,
  3B 3/8, 0.5B 0/8. Expect the cliff; find where it sits for your models.
- **Never trade model size for speed.** A fast wrong answer costs more than the tokens it saved.
- **Judgment stays with you.** Local models run ~67% on judgment against ~99% on grounded lookups,
  in an identical confident tone — the answer text carries no signal about which you got.
- **Deterministic search beats a local model on code.** grep won twice here, against both an LLM
  file-filter and embedding search.
- **Capability tags are claims, not facts.** Send a real image, run a real query. This repo got
  vision wrong in *both* directions by trusting tags.
- **Budget ~60% of memory** for one model. The KV cache and anything already resident must fit too.

## Three tiers

You are the expensive one. Push work down until quality stops holding.

| Tier | Give it | Never give it |
|---|---|---|
| **You** (frontier) | judgment, design, review, anything where being wrong is costly | bulk reading, mechanical transforms |
| **Cheap tier** (a small/fast hosted model) | verifying a claim against cited evidence, reconciling fan-out, summarising many results | anything needing local machine state |
| **Local Ollama** (free) | embeddings, bulk transforms, grounded lookups on a large model, OCR | judgment, anything under ~12B |

A delegated answer costs ~150 tokens whatever the input size, so past ~1k tokens of source it
already wins. **The decision is about your context budget, not the question.**

**Verify with the cheap tier, not the local one.** A local model cannot check its own work — which
is why `delegate_research` returns a verdict computed from what the run *did* (files read, model
used), never from what the model claims. On `verify` or `escalate`, the cheapest correct move is a
small-model pass over the cited evidence. Reserve yourself for when that pass disagrees.

## Order of operations

1. **`doctor`** — first, and on any connection or timeout error.
2. **`models {view:"resident"}`** — before any large call. A `placement.warning` means the model
   spilled to CPU: many times slower, no error raised.
3. **`route` with `minConfidence:"medium"`** — for anything quality-sensitive. It grades `medium`
   only with BOTH a `--policy-file` and a `--benchmark-report`; with neither, everything is `low`
   and the gate refuses everything. `freellama policy-from-eval` generates the policy from quality
   data — see `references/model-selection.md`.
4. **`run_task`** for generation from content you supply; **`delegate_research`** when it must read
   files. `run_task` is not grounded and has been observed inventing facts when given none.
5. **`ollama_manage`** to pull or unload; **`ollama_delete`** only when a human names the model.

## Hard rules

- Never co-resident two large models without checking the arithmetic first.
- Never delete model files under `~/.ollama/models` directly — only `ollama rm` / `ollama_delete`,
  and never on a staleness heuristic. Report candidates; a human decides.
- `scripts/check.sh` is read-only. Surface problems, don't silently fix them.
- Check for straggler runs before starting a new one against a shared output directory.
- Retry/backoff/timeout changes to `packages/rust-core/src/proxy.rs` need a failing test in `packages/rust-core/tests/proxy_contract.rs`
  first.

## Where to read more

| Question | Reference |
|---|---|
| What should I offload, and what is cheapest? | `references/task-delegation.md` |
| Local RAG / semantic search over a corpus | `examples/local-rag.sh` (runnable) |
| Which model, and how do I configure Ollama? | `references/model-selection.md` |
| Something is broken | `references/troubleshooting.md` |
| `proxy` or `serve`? | `references/proxy-vs-serve.md` |
| Retry/backoff behaviour | `references/reliability.md` |
| Disk is full | `references/disk-cleanup.md` |
| Deep profile of one model | `references/model-profile-qwen3.8-27b-mlx.md` |

`scripts/check.sh` runs the same health audit as `doctor` for a human at a terminal with no agent
attached. Exit 0 means every required check passed.

## Ollama itself — authoritative sources

Check these rather than trusting cached knowledge; Ollama's defaults change between releases and
several of them are not what they appear.

| Topic | Source |
|---|---|
| All env vars, live, for the installed build | `ollama serve --help` — the only source guaranteed to match your binary |
| Concurrency and memory defaults | https://github.com/ollama/ollama/blob/main/docs/faq.mdx |
| Context-length defaults (VRAM-tiered) | https://github.com/ollama/ollama/blob/main/docs/context-length.mdx |
| Env var declarations | https://github.com/ollama/ollama/blob/main/envconfig/config.go |
| How those declarations actually resolve | https://github.com/ollama/ollama/blob/main/server/sched.go |
| HTTP API | https://github.com/ollama/ollama/blob/main/docs/api.md |

**Read `envconfig` and `sched.go` together.** `OLLAMA_MAX_LOADED_MODELS` is declared as `0` in
envconfig, which reads as "unlimited" — but `sched.go` resolves that sentinel to
`defaultModelsPerGPU * gpu_count`, i.e. **3** on a single-GPU machine. This project shipped the
wrong advisory for weeks by reading only the first file. A declaration is not a resolved value.
