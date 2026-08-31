---
name: freellama
description: "Use when you need to offload computation to free local models — image/vision transcription and OCR, grounded code research over real files, embeddings and bulk transforms — instead of spending your own context. Also for driving the FreeLlama MCP: routing with evidence, checking admission/queue state before delegating, choosing or installing a model, or diagnosing Ollama/FreeLlama failures (503 server busy, 422 refusals, timeouts, proxy vs serve)."
---

# FreeLlama

How any orchestrating agent should drive the FreeLlama MCP server: push work down to free local
models without letting a weak model quietly get it wrong.

Flow: `health → doctor → models(resident) → route → run_task | delegate_research → read the verdict`

**Should I delegate at all?** One GET answers it:
`/_freellama/v1/health` returns `admission.slots_available` — `0` means "expect to queue up to
`max_queue_wait_seconds`". Delegate when the source is large (past ~1k tokens the maths wins) and
slots are free; do it yourself when the lookup is trivial, the source is already in your context,
or you would rather not wait out the queue. That check is advisory and racy by design — it is a
load-shedding signal, not a reservation.

## Discover this machine before trusting any number here

Model names, sizes, and scores in these references come from **one** setup. Yours will differ.
Three calls give you the real picture:

```
doctor                      memory, chip, and the 9 OLLAMA_* settings with effective defaults
models {view:"installed"}   what you have: capabilities, advertised context, policy rank
models {view:"resident"}    what is loaded now, and its GPU/CPU split
```

If nothing installed fits: `search_models` (search the library → inspect a family's tags → memory
fit against *your* machine). There is no `recommend` tool — that route exists over HTTP and in the
CLI (`freellama recommend`) but was deliberately removed from the MCP surface; `search_models` is
the agent-facing replacement.

## Rules that hold on any machine

- **Accuracy collapses below roughly 12B** for grounded code research. Measured here: 7B 2/8,
  3B 3/8, 0.5B 0/8. Expect the cliff; find where it sits for your models.
- **Never trade model size for speed.** A fast wrong answer costs more than the tokens it saved.
- **Judgment stays with you.** Local models run ~67% on judgment against ~99% on grounded lookups,
  in an identical confident tone — the answer text carries no signal about which you got.
- **Deterministic search beats a local model on code.** grep won twice here, against both an LLM
  file-filter and embedding search.
- **Tool output is paginated, not truncated.** The adapters keep every byte and show the model one
  page plus an exact next-page action, so "not found" is only trustworthy once the pages it needed
  were actually read. Clipping used to discard ~89% of a routine repo-wide grep, which is how a
  model concludes something is absent from a window that never contained it.
- **Capability tags are claims, not facts.** Send a real image, run a real query. This repo got
  vision wrong in *both* directions by trusting tags.
- **Budget ~60% of memory** for one model. The KV cache and anything already resident must fit too.

## Three tiers — and the small model is the one holding these tools

    LARGE MODEL  <->  SMALL MODEL (drives this MCP)  <->  LOCAL OLLAMA (vision / code / embeddings)

You are the expensive one. Push work down until quality stops holding.

| Tier | Give it | Never give it |
|---|---|---|
| **1 — Large** (frontier) | judgment, design, review, deciding what matters, anything where being wrong is costly | bulk reading, raw vectors, and — where you can arrange it — these tool schemas themselves |
| **2 — Small** (a small/fast model, ideally the one holding this MCP) | tool dispatch, verifying a claim against cited evidence, reconciling fan-out, summarising many results | real judgment; anything needing local machine state it cannot query |
| **3 — Local Ollama** (free) | embeddings, bulk transforms, grounded lookups on a large model, OCR | judgment, anything under ~12B, "which file is relevant" (grep wins) |

**Why tier 2 should hold these tools rather than tier 1.** The eight schemas cost ~2,990 tokens,
billed on *every* turn of a session whether or not you call one. On a frontier orchestrator that is
a standing tax; inside a small model's bounded sub-session it is paid once. And tool dispatch —
pick the tool, fill the path, read the verdict, retry once — is mechanical routing, not judgment,
which is exactly what small models are good at. If your harness cannot split the tiers, holding the
MCP yourself still works; you just pay the schema rent.

A delegated answer costs ~150-450 tokens whatever the input size. Measured on this repo: six
grounded questions whose sources total 59,208 tokens returned 1,742 — **97.1% offload**. Past ~1k
tokens of source it already wins. **The decision is about your context budget, not the question.**

**Scope the workspace you hand down.** Accuracy on the same six questions went 42% → 83% purely by
telling the agent to exclude `node_modules`/`target`/`.venv` and to distrust `fixtures/` and
`mocks/`. An unscoped delegation over a real repo reads a mock and cites it honestly.

**Verify with the cheap tier, not the local one.** A local model cannot check its own work — which
is why `delegate_research` returns a verdict computed from what the run *did* (files read, model
used), never from what the model claims. On `verify` or `escalate`, the cheapest correct move is a
small-model pass over the cited evidence. Reserve yourself for when that pass disagrees.

## Order of operations

1. **`doctor`** — first, and on any connection or timeout error.
2. **`models {view:"resident"}`** — before any large call. A `placement.warning` means the model
   spilled to CPU: many times slower, no error raised.
3. **`route` with `minConfidence:"medium"`** — for anything quality-sensitive. The gate lives in the
   router itself, so the CLI (`--min-confidence`), the HTTP API and anyone embedding
   `freellama-core` all inherit it — it is not an MCP-only courtesy. An unrecognised grade (e.g.
   `"high"`, which the router never issues) is refused rather than silently ignored.
   - **Do not read `confidence` as a probability.** It is derived from three dimensions the
     response reports separately: `quality_evidence` (a policy vouches for this model on this
     task), `task_evidence` (a functional benchmark measured it), `hardware_fit`
     (strong / insufficient_context / unknown). `medium` requires the first two both `strong`.
     `rejected[]` lists every losing candidate with its reason, so you can audit the comparison,
     not just the verdict.
   - With neither a `--policy-file` nor a `--benchmark-report`, everything grades `low` and the
     gate refuses everything. `freellama policy-from-eval` generates the policy from quality
     data — when picking or configuring a model, see `references/model-selection.md`.
4. **`run_task`** for generation from content you supply; **`delegate_research`** when it must read
   files. `run_task` is not grounded and has been observed inventing facts when given none.
   - **A managed task can be refused, and 503 is a real answer.** Admission is a budget in cost
     units — embedding 1, chat 2, vision 4, default 8 total — and a task that cannot get a slot
     within 120s returns `503 server busy` naming the cost and the budget. That is deliberate: it
     matches Ollama's own `ErrMaxQueue`, because waiting silently turns load into latency you
     cannot attribute. **Retry, or lower your fan-out — do not treat it as a failure.** Every
     success reports `admission.queue_wait_ms`, so you can see throttling before it becomes a 503.
   - **Read `delegate_research`'s `structuredContent`, not its prose.** `verification` gives
     recommendation + why; `citations[]` gives the full, unclipped commands and paths behind the
     answer.
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
