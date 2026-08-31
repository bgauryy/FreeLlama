# What to offload, what to keep, and how to read the answer

Load before choosing what to hand `run_task` or `delegate_research`, or before trusting an answer
unsupervised. Every claim below traces to a measurement in this repo unless marked as external.

## This is delegation, not distillation

**Knowledge distillation** is a *training-time* technique: collect a teacher model's outputs, then
fine-tune a student to mimic them, baking capability into weights
([Lightly](https://www.lightly.ai/blog/knowledge-distillation),
[Springer](https://link.springer.com/article/10.1007/s10462-025-11423-3)). FreeLlama does none of it
— no fine-tuning, no training-data collection, no student being updated.

**What `delegate_research` does is runtime task delegation**: the small model runs as-is and is
handed narrow, verifiable work it is already good at. Different cost profile: no training cost, no
permanent capability transfer — and no improvement to the small model either. Every session starts
from the same competence baseline in `references/model-profile-qwen3.8-27b-mlx.md`. Real distillation
(collecting verified question/answer pairs and fine-tuning) is separate, unbuilt work.

## The two offload tools

`route` / `doctor` / `models` / `search_models` only make *selection* decisions — they never do work.

- **`run_task`** routes and executes one chat/generate/embed/vision call in one shot from content
  **you** supply. The general-purpose path. Use it by default.
- **`delegate_research`** hands a question to a local model wired to a shell (or `octocode`) agent
  inside `workspacePath` and returns a grounded, citable answer with an evidence trail — at the cost
  of a heavier subprocess spawn. Reach for it specifically when you need the citable trail.

Verified end-to-end: a real question about FreeLlama's own [`Cargo.toml`](https://github.com/bgauryy/FreeLlama/blob/main/Cargo.toml) answered correctly in one
call, 8K tokens spent entirely on the local model, zero cost to the orchestrator's context.

## Delegate freely — verified, high confidence

- **Grounded code search** ("where is X", "what does Y do", "every call site of Z") — 98.9% measured
  accuracy across 100+ real questions on real codebases, because the model must cite `file:line`,
  which makes wrongness cheap to catch and mostly self-correcting.
- **Long-context fact retrieval** — correct on an 11K-token file with the fact buried mid-file, 7.4s.
- **Honesty-sensitive lookups** ("does X exist") — refuses to fabricate under direct pressure,
  tested with a nonexistent function name and a fictional historical event.
- **Summarization, refactoring suggestions, unit-test generation, doc drafts** — not directly tested
  here, but retrieval- and generation-shaped rather than judgment-shaped, and consistent with
  general guidance on using large local context windows for exactly this
  ([nOps](https://www.nops.io/blog/llm-cost-optimization-tips/)).

## Delegate only with mandatory verification

- **Code review / bug-finding** — ~67% real accuracy. It hallucinated a nonexistent control-flow bug
  and misstated a well-known crate's API, **in the same confident tone as its correct findings**.
  Use it to generate review candidates, never to approve them.
- **Multi-step arithmetic or logic with a twist** — 3 errors out of 100 challenges, all in this
  category. Single-step arithmetic was fine; two chained computations were where it broke.
- **Anything picked by a zero-config `route --objective fastest`** — it selects on capability
  metadata alone. → `references/model-selection.md`

## Don't bother delegating

- **Trivial work.** A question answerable from context in hand, or a one-line lookup, is not worth a
  subprocess spawn, model load, and tool-call round trip (~10-100s and several thousand tokens on
  the local side). Delegation overhead is real and externally documented: agent teams use ~7× more
  tokens than standard sessions, and un-optimized multi-agent systems 4-15× more than single calls
  ([MindStudio](https://www.mindstudio.ai/blog/ai-orchestrator-cheaper-sub-agent-models)).
- **Reasoning without `think:true`.** With thinking disabled the model guesses fast and wrong on
  anything multi-step (verified: instant wrong answers on math word problems).
  `delegate_research`'s adapter already forces the right mode for grounded search — do not reuse
  that pattern for pure reasoning without checking which mode is active.

## What the local model is genuinely cheapest at

Ranked by value per second. The ordering matters more than the numbers: **the cheapest work is the
work that is not a generation task.**

| Work | Measured | Verdict |
|---|---|---|
| **Embeddings** via `run_task` | 322 chunks / 159k local tokens in **9.6s** (30ms/chunk), 0 tokens returned | Strongest use by a wide margin — no sampling, so nothing to hallucinate |
| Near-duplicate / clustering | 96 documents embedded in **2.9s** (31ms/doc) | Strong. Finds overlap with *no keyword*, which `grep` cannot do |
| Image work | ~10-45s per image | Works, but not fast |
| One grounded question | 7-62s for ~150-450 tokens back | Worth it past ~1k tokens of source |
| **Semantic search over code** | **Lost to `grep`** | **Do not use** |

### The negative result, recorded so it is not rediscovered

Two attempts to use a local model for *finding relevant code* both lost to plain `grep`:

- **Local model as a file filter**: 4/6 recall across 153 files, 24-39s. `grep` is exact and
  instant. `gemma4:12b-mlx` and `qwen3.8:27b-mlx` scored the *same* 4/6 — ranking files is not what
  model size buys you.
- **Embedding search**: over 152 chunks with 6 questions and known-correct files, `nomic-embed-text`
  scored 5/6 recall@3. Re-run over 242 chunks of this repo's Rust source, top-1 got 2 of 3.

The failure is worth more than the successes. *"How does it avoid loading two models into memory"*
returned `lib.rs` instead of `platform/mod.rs`, and has now missed in two independent runs, because
`platform/mod.rs` never uses that phrase — it says `managed_execution`, `RwLock`, "admission
permit". **Embeddings match how text is phrased, and code routinely names a concept in vocabulary
that looks nothing like the question.**

So the honest boundary is narrower than "embeddings lose to grep": **`grep` wins when you know the
keyword, which for code you usually do. Embeddings win when there is no keyword** — grouping,
deduplication, classification, similarity. Same lesson the octocode-vs-bash benchmark recorded from
the other direction: the structured search tool lost to raw shell on every model tested.

### Similarity is a candidate, not a verdict

The duplicate-document scan rated two of this repo's docs at 0.949 similarity. Inspection showed
they are not redundant at all — one documents admission permits, the catalog cache, and the product
boundary, none of which appear in the other. High cosine similarity means *"about the same
subject"*, not *"one can be deleted"*. Read the candidates before acting on a score.

### Local RAG, if you actually need it

`examples/local-rag.sh` is a working ~40-line pattern: `npx freellama task --task embedding
--input-file` produces the vectors, `jq` does cosine and top-k. **FreeLlama owns no vector store**,
deliberately — persistence is a standing non-goal, and a stale index fails *silently*, returning
confidently wrong files as the corpus drifts. You own storage and staleness; swap the flat file for
sqlite-vec or LanceDB when it outgrows one.

## Reading a `delegate_research` result — use the structured half

| Field | What it gives you |
|---|---|
| `verification.recommendation` | `accept` / `verify` / `escalate` — see the verdict table in `SKILL.md` |
| `verification.why` | the reason, computed from what the run did |
| `citations[]` | `{step, tool, path, command}` per **successful** step — full, unclipped |
| `successfulToolCallCount` vs `toolCallCount` | how many steps actually read something |

`summary` carries the same information as prose. Two independent small-model callers asked for the
structured triple instead, so prefer it: no parsing, and nothing lost.

**`citations[].command` is deliberately not truncated.** It used to be head-sliced at 120
characters, cutting commands mid-flag — a real trail came back ending
`--exclude-dir={node_modules,target,.venv,__pycach`, unauditable at exactly the moment auditing
matters. Only the prose line clips now, and it states how many characters it dropped. Note the
causality: the search-scoping guidance added to the adapters made commands *longer*, which is what
pushed them past the old limit — a fix in one place surfacing a latent bug in another.

Failed steps are excluded from `citations[]` on purpose: a command that errored is not a citation for
anything. Same rule the verdict uses — grounding counts successful calls only.

## Practical rule of thumb

> Delegate anything the local model can *prove* by citing evidence you can check in one glance.
> Keep anything needing judgment, synthesis across ambiguous tradeoffs, or multi-step reasoning
> without visible work — or verify it exhaustively before trusting it.

## Cost-reduction hierarchy (external best practice, not tested here)

Per current guidance ([Wavect](https://wavect.io/blog/reduce-llm-token-costs-2026/),
[Obvious Works](https://www.obviousworks.ch/en/token-optimization-saves-up-to-80-percent-llm-costs/)):

1. **Prompt caching** first (up to ~90% off cached input) — applies to *your* calls, not the local
   model's.
2. **Confidence-gated waterfall routing** — local first, escalate when the answer looks uncertain or
   the task is outside the verified-good zone above. Reported: ~95% of frontier quality at 75-85%
   lower cost.
3. **Right-sizing** — open-weight local models run 15-30× cheaper per token when the task is
   genuinely in their competence zone, which is why getting that zone right matters more than the
   raw ratio.

Next: per-field evidence behind every claim here → `references/model-profile-qwen3.8-27b-mlx.md`.
Choosing the model that does the work → `references/model-selection.md`.
