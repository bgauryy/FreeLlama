---
name: freellama
description: "Use when pushing token-heavy work onto free local Ollama models through FreeLlama instead of spending orchestrator context: grounded code research over real files, image/OCR transcription, embeddings or a local semantic index, bulk transforms. Also use when deciding whether delegating is worth it at all; when picking, sizing, or installing a local model; and when diagnosing a FreeLlama or Ollama failure — 503 server busy, queue waits, a route refused for low confidence, a model spilled to CPU, proxy-vs-serve 404s, or a disk full of models."
---

# FreeLlama

How an orchestrating agent drives the [FreeLlama](https://github.com/bgauryy/FreeLlama) MCP server:
push work down to free local models without letting a weak model quietly get it wrong.

Eight MCP tools: `doctor`, `models`, `route`, `search_models`, `run_task`, `ollama_manage`,
`ollama_delete`, `delegate_research`. Without an MCP client, `npx freellama tools` prints each one
beside its CLI equivalent — `search_models` and `delegate_research` are MCP-only; `serve`, `proxy`,
`bench-all`, `policy-from-eval`, `eval` and `recommend` are CLI-only. `npx freellama doctor` is the
only command that runs with `serve` down.

Flow: `health (delegate at all?) → doctor → models{resident} → pick a flow (A-E) → read the verdict`

**Pick a flow below rather than calling tools ad hoc.** Every flow ends in a signal you must read —
a verdict, a placement warning, a refusal — and skipping that read is how a weak answer gets used.

## Step 0 — should you delegate at all?

One GET answers it: `/_freellama/v1/health` returns `admission.slots_available`. `0` means "expect
to queue up to `max_queue_wait_seconds`" (default 120). It is a snapshot — advisory and racy by
design: a load-shedding signal, not a reservation.

| Delegate | Do it yourself |
|---|---|
| Source past ~1k tokens, and slots are free | The lookup is trivial, or the source is already in your context |
| Privacy-bound or rate-limited work — delegate regardless | You need judgment: review, design, "is this good" |
| Embeddings, bulk transforms, OCR, grounded lookups | You would rather not wait out the queue (7-62s per grounded question) |

A delegated answer costs ~150-450 tokens whatever the input size. Measured: six grounded questions
whose sources total 59,208 tokens returned 1,742 — **97.1% offload**. **The decision is about your
context budget, not the question.**

## Discover this machine before trusting any number here

Model names, sizes, and scores in this skill come from **one** setup (M4 Pro, 52GB). Yours differ.

```
doctor                      memory, chip, and the 9 OLLAMA_* settings with effective defaults
models {view:"installed"}   what you have: capabilities, advertised context, policy rank
models {view:"resident"}    what is loaded now, and its GPU/CPU split
```

## Flow A — a question about real files (`delegate_research`)

1. `models {view:"resident"}` — a `placement.warning` means the model spilled to CPU: many times
   slower, no error raised. Skip if you checked recently.
2. `delegate_research {question, workspacePath, model?, adapter?}`
   - `workspacePath` must be an absolute, existing directory **inside
     `FREELLAMA_MCP_ALLOWED_ROOTS`** (defaults to the FreeLlama checkout). Outside it, the call
     fails and names the allowed roots — this boundary exists because an unconstrained version was
     verified happily listing a real `$HOME`.
   - `adapter` defaults to `bash`, which beat `octocode` on every model measured and is ~3× faster.
     Ask for `octocode` only when you specifically want structured search.
   - **Scope the question.** Say what to exclude (`node_modules`, `target`, `.venv`) and that
     `fixtures/` and `mocks/` are not evidence. Accuracy on the same six questions went
     **42% → 83%** on that instruction alone; unscoped, it reads a mock and cites it honestly.
   - Best shape: one narrow, self-contained question over 1-5 files.
3. Read `structuredContent`, not the prose: `verification` (recommendation + why), `citations[]`
   (full, unclipped commands and paths), `successfulToolCallCount` vs `toolCallCount`.
4. Act on the verdict — it is computed from what the run *did* and which model ran, never from the
   model's self-report:

| Verdict | What actually triggered it | Do |
|---|---|---|
| `escalate` | the model is graded unusable here (refused pre-flight, nothing ran), **or** zero *successful* tool calls — parametric recall, not research | Don't use the answer |
| `verify` | no measured grade for this model; or graded weak; or the question reads as judgment (keyword heuristic); or more than 5 successful calls, outside the measured 1-5 envelope | One cheap-tier pass over `citations[]` |
| `accept` | grounded in 1-5 successful calls, lookup-shaped, model measured strong | Use it |

Grades are loaded from disk, never compiled in
([`benchmark/evidence/model-evidence.json`](https://github.com/bgauryy/FreeLlama/blob/main/benchmark/evidence/model-evidence.json),
override with `FREELLAMA_MCP_MODEL_EVIDENCE`). No entry means unmeasured, which yields `verify` —
the correct answer when nothing is known.

5. On `isError`, retry once, then answer it yourself. Failed steps are excluded from `citations[]`
   on purpose: a command that errored is not a citation for anything.

**Verify with the cheap tier, not the local one.** A local model cannot check its own work.
Reserve yourself for when the cheap pass disagrees.
When deciding *what* to hand down at all, load `references/task-delegation.md`.

## Flow B — generation, transform, or vision from content you already hold (`run_task`)

`run_task {task, prompt | messages, images?, model?, keepAlive?, minConfidence:"medium"}`

- **Not grounded**: no file access, and verified inventing facts about this project when given
  none. If it must read a file, that is Flow A.
- `images`: base64, **no** data-URI prefix. Name an explicit vision model — capability tags are
  claims, not facts, and this project got vision wrong in *both* directions by trusting them.
- `minConfidence` is enforced by a free `route` preview *before* anything runs. Gating afterwards
  would be useless: by the time `run_task` returns, the tokens are spent.
- `keepAlive:"0"` unloads immediately after the call, `"-1"` pins, default 5m.

## Flow C — embeddings and a local index (the cheapest work here)

`run_task {task:"embedding", input:[...]}` — **batch it**; batching is far cheaper than one call
per item. `returnEmbeddings` defaults to `false`, so the raw vectors never enter your context, and
that withholding is most of the saving. An embedding costs 1 admission unit against chat's 2, so it
is also the least likely call to queue.

Measured: 322 chunks / 159k local tokens in 9.6s (30ms/chunk), 0 tokens returned. No sampling, so
nothing to hallucinate — by a wide margin the strongest use of a local model.

Reach for it when there is **no keyword**: grouping, deduplication, classification, similarity.
When you can guess the identifier, `grep` wins on accuracy, latency and cost at once — never use a
local model to pick which files are relevant. `examples/local-rag.sh` is a runnable end-to-end
pattern. For what else is cheap here and what lost to `grep`, load `references/task-delegation.md`.

## Flow D — choosing or installing a model

1. `models {view:"installed"}` — never assume a name; estates differ per machine.
2. Nothing fits → `search_models`, **two steps, both required**: omit `model` for family names,
   then pass `model:"<family>"` for its tags with size, context window and `fitsInMemory` computed
   against this machine. A family is not pullable. Pulling from step 1 alone is guessing the size,
   which is how a 143GB tag looked like a candidate on a 48GB box. Step 2 fails closed: with
   `serve` down there is no machine profile, so nothing is recommended and you get
   `recommendationUnavailable` — start `serve`, do not pick the biggest tag yourself.
3. `models {view:"detail", model}` for the true max context and quantization.
4. `ollama_manage {action:"pull", model}` — real multi-GB download, only after a human approves.
5. Quality-sensitive pick? `route` with `minConfidence:"medium"` first. When it refuses everything,
   load `references/model-selection.md` — that gate needs configuring before it can pass.

There is no `recommend` MCP tool. It exists over HTTP (`POST /_freellama/v1/recommendations`) and in
the CLI (`freellama recommend`), and was deliberately kept off the MCP surface; `search_models` is
the agent-facing replacement.

## Flow E — something is broken

1. `doctor` — first, and on any connection or timeout error.
2. `models {view:"resident"}` — placement warning? more than one large model?
3. `scripts/check.sh` — the same audit for a human at a terminal with no agent attached. Exit 0
   means every required check passed. Read-only: it surfaces problems, never fixes them.
4. When you have a symptom and no cause yet, load `references/troubleshooting.md`.

**`503 server busy` is a real answer, not a failure.** Admission is a budget in cost units —
embedding 1, chat 2, vision 4, 8 total by default — and a task that cannot get a slot within 120s
is refused, naming the cost and the budget. That is deliberate: it matches Ollama's own
`ErrMaxQueue`, because waiting silently turns load into latency you cannot attribute. **Retry, or
lower your fan-out.** Every success reports `admission.queue_wait_ms`, so throttling is visible
before it becomes a 503.

## Reading `route`'s answer

`route` is free and generates nothing. Skip it before `run_task`, which routes internally.

**Do not read `confidence` as a probability.** It is derived from three dimensions reported
separately: `quality_evidence` (a policy vouches for this model on this task), `task_evidence` (a
functional benchmark measured it), `hardware_fit` (`strong` / `insufficient_context` / `unknown`).
`medium` requires the **first two** both `strong` — `hardware_fit` does not feed it. `rejected[]`
lists every losing candidate with its reason, so the comparison is auditable, not just the verdict.

The gate lives in the router, so the CLI (`--min-confidence`), the HTTP API and anything embedding
`freellama-core` inherit it. An unrecognised grade — `"high"`, which the router never issues — is
refused rather than ignored, because an ignored floor looks exactly like a satisfied one. With
neither a policy file nor a benchmark report, **everything grades `low` and the gate refuses
everything**; when you need `medium` to pass, load `references/model-selection.md`.

## Rules that hold on any machine

- **Accuracy collapses below roughly 12B** for grounded code research. Measured here: 7B 2/8,
  3B 3/8, 0.5B 0/8. Expect the cliff; find where it sits for your models.
- **Never trade model size for speed.** A fast wrong answer costs more than the tokens it saved.
- **Judgment stays with you.** Local models run ~67% on judgment against ~99% on grounded lookups,
  in an identical confident tone — the answer text carries no signal about which you got.
- **Deterministic search beats a local model on code.** `grep` won twice here, against both an LLM
  file-filter and embedding search.
- **"Not found" is only trustworthy once the pages it needed were read.** Adapter output is
  paginated, not truncated — every byte is kept and the model gets one page plus an exact next-page
  action. Clipping used to discard ~89% of a routine repo-wide grep, which is how a model concludes
  something is absent from a window that never contained it.
- **Budget ~60% of memory** for one model. The KV cache and anything already resident must fit too.
- **A declaration is not a resolved value.** `OLLAMA_MAX_LOADED_MODELS` is declared `0` ("unlimited")
  but resolves to 3 on a single-GPU machine. Reading only the declaration shipped a wrong advisory
  here for weeks. Before changing any `OLLAMA_*` value, load `references/ollama-config.md`.

## Three tiers — and the small model is the one holding these tools

    LARGE MODEL  <->  SMALL MODEL (drives this MCP)  <->  LOCAL OLLAMA (vision / code / embeddings)

You are the expensive one. Push work down until quality stops holding.

| Tier | Give it | Never give it |
|---|---|---|
| **1 — Large** (frontier) | judgment, design, review, deciding what matters, anything where being wrong is costly | bulk reading, raw vectors, and — where you can arrange it — these tool schemas themselves |
| **2 — Small** (ideally the model holding this MCP) | tool dispatch, verifying a claim against cited evidence, reconciling fan-out, summarising many results | real judgment; anything needing local machine state it cannot query |
| **3 — Local Ollama** (free) | embeddings, bulk transforms, grounded lookups on a large model, OCR | judgment, anything under ~12B, "which file is relevant" (`grep` wins) |

The eight schemas cost ~2,990 tokens, billed on *every* turn whether or not you call one — a
standing tax on a frontier orchestrator, paid once inside a small model's bounded sub-session. And
tool dispatch (pick the tool, fill the path, read the verdict, retry once) is mechanical routing,
not judgment. If your harness cannot split the tiers, holding the MCP yourself still works; you just
pay the schema rent.

## Hard rules

- Never co-resident two large models without checking the arithmetic first.
- Never delete model files under `~/.ollama/models` directly — only `ollama rm` / `ollama_delete`,
  and never on a staleness heuristic. Report candidates; a human decides.
- `ollama_manage {action:"pull"}` and `ollama_delete` need explicit human approval, per call.
- Check for straggler background runs before starting a new one against a shared output directory.
- Contributing to FreeLlama itself: retry/backoff/timeout changes to
  [`proxy.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/proxy.rs) need
  a failing test in
  [`proxy_contract.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/tests/proxy_contract.rs)
  first.

## Where to read more

| Question | Load |
|---|---|
| What should I offload, what is cheapest, how do I read a result? | `references/task-delegation.md` |
| Which model, and how do I make `minConfidence:"medium"` reachable? | `references/model-selection.md` |
| Will it fit in memory? What do the `OLLAMA_*` vars actually default to? | `references/ollama-config.md` |
| Something is broken (symptom → cause → fix) | `references/troubleshooting.md` |
| `proxy` or `serve`? Which routes exist in each? | `references/proxy-vs-serve.md` |
| Retry, backoff, timeout, process restart | `references/reliability.md` |
| Disk is full; what may I delete? | `references/disk-cleanup.md` |
| Deep evidence for one model, and how to profile another | `references/model-profile-qwen3.8-27b-mlx.md` |
| Runnable local RAG in ~40 lines | `examples/local-rag.sh` |
| Human-facing health audit, no agent attached | `scripts/check.sh` |

## Ollama itself — authoritative sources

Check these rather than trusting cached knowledge; Ollama's defaults change between releases and
several are not what they appear.

| Topic | Source |
|---|---|
| All env vars, live, for the installed build | `ollama serve --help` — but only authoritative when CLI and server are the same build; `doctor` reports that mismatch |
| Concurrency and memory defaults | https://github.com/ollama/ollama/blob/main/docs/faq.mdx |
| Context-length defaults (VRAM-tiered) | https://github.com/ollama/ollama/blob/main/docs/context-length.mdx |
| Env var declarations | https://github.com/ollama/ollama/blob/main/envconfig/config.go |
| How those declarations actually resolve | https://github.com/ollama/ollama/blob/main/server/sched.go |
| HTTP API | https://github.com/ollama/ollama/blob/main/docs/api.md |
