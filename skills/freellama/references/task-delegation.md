# Task delegation: what to offload to the local model, guided by the big LLM

Load before choosing what to hand the `delegate_research` MCP tool, or before trusting its answer
unsupervised.

This is the decision framework for any large/frontier orchestrating model (etc.) orchestrating a local
model via `packages/mcp`'s tools. It's grounded primarily in evidence gathered *in this repo, this
session* — not just general industry practice — with external research filling in the parts not
directly tested here.

## This is delegation, not distillation — they're different techniques

**Knowledge distillation** is a *training-time* technique: collect a large "teacher" model's
outputs on many examples, then fine-tune a smaller "student" model to mimic them, permanently
baking some of the teacher's capability into the student's weights
([Lightly](https://www.lightly.ai/blog/knowledge-distillation),
[Springer](https://link.springer.com/article/10.1007/s10462-025-11423-3)). FreeLlama does none of
this today — there's no fine-tuning pipeline, no training data collection, no student model being
updated.

**What `delegate_research` does is runtime task delegation**: the small model runs as-is, un-modified,
and simply gets handed narrow, verifiable work it's already good at — no training involved. This is
sometimes described loosely as "distillation-style usage" in casual conversation, but it's a
different technique (orchestration/routing) with a different cost profile: no training cost, no
permanent capability transfer, but also no improvement to the small model itself — every session
starts from the same competence baseline documented in `model-profile-qwen3.8-27b-mlx.md`.

If you actually wanted distillation (a small model that gets *better* over time at your specific
tasks), that's separate, unbuilt work: collecting (question, verified-correct-answer) pairs from
sessions like this one and fine-tuning on them — plausible future work, not something delegation
alone gives you.

## Two offloading tools: `run_task` and `delegate_research`

`route`/`recommend`/`doctor`/`models` only make a *selection* decision — they never do work.
Two other tools do: `run_task` routes and executes an ordinary chat/completion/embedding call in
one shot — the general-purpose offload path. `delegate_research` is the specialized one: it hands
a question to a local model wired up with the `octocode` CLI and returns a grounded, citable
answer with an evidence trail, at the cost of a heavier subprocess spawn. Use `run_task` by
default; reach for `delegate_research` specifically when you need the citable evidence trail (a
code-research question you want to be able to spot-check) rather than just an answer.
Verified end-to-end: a real question about this repo's own `Cargo.toml` answered correctly in one
call, 8K tokens spent entirely on the local model, zero cost to the orchestrator's own context.

## Delegate freely (verified, high confidence)

- **Grounded code search / "where is X" / "what does Y do" / "find every call site of Z"** —
  98.9% measured accuracy across 100+ real questions on real codebases (`model-profile-qwen3.8-27b-mlx.md`),
  because the model must cite file:line evidence, which makes wrongness cheap to catch and mostly
  self-correcting (a wrong citation is obviously wrong on inspection).
- **Long-context fact retrieval** — verified correct on an 11K-token file with a specific fact
  buried in the middle, 7.4s.
- **Honesty-sensitive lookups** ("does X exist") — verified it refuses to fabricate under direct
  pressure (a nonexistent function name, a fictional historical event).
- **Summarization, refactoring suggestions, unit-test generation, documentation drafts** —
  not directly tested in this repo, but consistent with the accuracy pattern above (retrieval- and
  generation-heavy, not judgment-heavy) and matches general industry guidance: "leverage massive
  local context windows for development tasks such as refactoring, generating unit tests, and
  documentation" ([nOps](https://www.nops.io/blog/llm-cost-optimization-tips/)).

## Delegate with mandatory verification (real failure modes measured here)

- **Code review / bug-finding** — only ~67% real accuracy on this model: it hallucinated a
  nonexistent control-flow bug and made a factual error about a well-known crate's API, **stated
  with the same confident tone as its correct findings**. Use it to generate review candidates,
  never to approve them. Re-derive the ground truth for anything it flags before acting on it.
- **Multi-step arithmetic/logic with a "twist"** (re-splitting a bill, an age-ratio puzzle) — 3
  real errors out of 100 challenges, all in this category. Simple lookups and single-step
  arithmetic were fine; problems requiring two chained computations were where it broke.
- **Anything where `route --objective fastest` picked the model with no policy configured** —
  verified to pick by capability metadata alone (a 0.5B model was selected for code-repair). Only
  trust `balanced`/`quality` routes backed by an evaluated policy, or an explicitly named model.

## Don't bother delegating

- **Trivial tasks cheaper to just do inline.** Research confirms multi-agent/delegation overhead
  is real: "agent teams use ~7x more tokens than standard sessions" and un-optimized multi-agent
  systems can consume "4-15x more tokens than simple single calls"
  ([MindStudio](https://www.mindstudio.ai/blog/ai-orchestrator-cheaper-sub-agent-models)). A
  question answerable from context already in hand, or a one-line lookup, isn't worth a
  subprocess spawn + model load + tool-call round trip (this repo's own measurements: ~10-100s and
  several thousand tokens *on the local side* per `delegate_research` call). Delegate when the
  question requires reading real files the orchestrator doesn't already have loaded — that's where
  the token savings (on the *orchestrator's* side) actually materialize.
- **Reasoning tasks without `think:true`.** A local model call with thinking disabled will guess
  fast and wrong on anything requiring actual multi-step reasoning (verified: instant wrong
  answers on math word problems with `think:false`). `delegate_research`'s underlying adapter
  already forces the right mode for grounded search; don't reuse the same pattern for pure
  reasoning tasks without checking which mode is active.

## Cost-reduction hierarchy (external best practice, not yet tested here)

Applies above and beyond task-type delegation, per current guidance
([Wavect](https://wavect.io/blog/reduce-llm-token-costs-2026/),
[Obvious Works](https://www.obviousworks.ch/en/token-optimization-saves-up-to-80-percent-llm-costs/)):

1. **Prompt caching** first (up to ~90% off cached input) — applies to the orchestrator's own
   calls, not the local model.
2. **Confidence-gated waterfall routing** — try the cheap/local model first, escalate to the
   frontier model only when the local answer looks uncertain or the task is out of the local
   model's verified-good zone above. Reported industry result: ~95% of frontier quality at
   75-85% lower cost.
3. **Right-sizing**: open-weight local models run "15 to 30x cheaper per token" than frontier
   APIs when the task is actually in their competence zone — which is exactly why getting the
   competence zone right (the sections above) matters more than the raw cost ratio.

## Practical rule of thumb

> Delegate anything the local model can *prove* by citing evidence you can check in one glance.
> Keep anything requiring judgment, synthesis across ambiguous tradeoffs, or multi-step reasoning
> without visible work — or verify it exhaustively before trusting it, exactly as this whole
> document was built: every claim above traces back to a specific test in this session, not to
> the model's or a vendor's own claims about itself.

Next: see `references/model-profile-qwen3.8-27b-mlx.md` for the underlying per-field evidence, or
`references/disk-cleanup.md` for the human-approval rule `ollama_delete` shares with this tool.

## Sources

- [How to Cut LLM Token Costs in 2026: Routing, Caching, Compression, and the Right Model](https://wavect.io/blog/reduce-llm-token-costs-2026/)
- [How to Build an AI Orchestrator That Delegates to Cheaper Sub-Agent Models](https://www.mindstudio.ai/blog/ai-orchestrator-cheaper-sub-agent-models)
- [Token optimization 2026: Saving up to 80% LLM costs](https://www.obviousworks.ch/en/token-optimization-saves-up-to-80-percent-llm-costs/)
- [LLM Cost Optimization: 10 Tips to Reduce AI Inference & Token Costs](https://www.nops.io/blog/llm-cost-optimization-tips/)
- `model-profile-qwen3.8-27b-mlx.md` (this skill) — the underlying verified evidence for every
  "delegate freely" / "verify first" claim above.

## What the local model is genuinely cheapest at — measured, and one negative result

Ranked by value per second on this machine. The ordering matters more than the numbers: the
cheapest work is the work that is *not a generation task*.

| Work | Measured | Verdict |
|---|---|---|
| **Embeddings** via `run_task` | 322 chunks / 159k local tokens in **9.6s** (30ms/chunk), 0 tokens returned | Strongest use by a wide margin — no sampling, so nothing to hallucinate |
| Near-duplicate / clustering | 96 documents embedded in **2.9s** (31ms/doc) | Strong. Finds overlap with *no keyword*, which grep cannot do |
| Image work | ~17-45s per image on `qwen3.8:27b-mlx` | Works, but not fast |
| One grounded question | 7-40s for ~150 tokens back | Worth it past ~1k tokens of source; see the economics above |
| **Semantic search over code** | **Lost to grep** | **Do not use** |

### The negative result, recorded so it is not rediscovered

Two separate attempts to use a local model for *finding relevant code* both failed against plain
`grep`:

- **Local model as a file filter**: 4/6 recall across 153 files, taking 24-39s. `grep` is exact and
  instant. `gemma4:12b-mlx` and `qwen3.8:27b-mlx` scored the same 4/6 — the larger model did not
  help, because ranking files is not the thing model size buys you.
- **Embedding search on a keyword-shaped question**: asked "how does it avoid loading two models
  into memory at the same time", the correct file did not appear in the top 3.

**Correction, from a larger sample.** That bullet rested on a *single* question. Re-run properly
over 152 chunks with 6 questions and known-correct files, `nomic-embed-text` scored **5/6
recall@3** — materially better than one failure suggested. The honest boundary is narrower than
"embeddings lose to grep": **grep wins when you know the keyword**, which for code you usually do.
Embeddings win when there is no keyword — grouping, deduplication, classification, similarity.

Model choice matters, and popularity does not predict it: `qwen3-embedding` ranks first on
ollama.com and scored **4/6 at 3.5x the indexing cost** of `nomic-embed-text` (5/6, 274MB).
`embeddinggemma:300m` tied on quality at twice the size.

This is the same lesson the octocode-vs-bash benchmark already recorded from the other direction:
the structured search tool lost to raw shell on every model tested. **For code, deterministic
search beats a local model on accuracy, latency, and cost simultaneously.** Reach for embeddings
when there is no keyword to search for — grouping, dedup, classification — not when there is.

### Caution: similarity is a candidate, not a verdict

The duplicate-document scan above rated `README.md` and `docs/ARCHITECTURE.md` at 0.949 similarity.
Inspection showed they are not redundant at all — ARCHITECTURE.md documents admission permits, the
30-second catalog cache, and the product boundary, none of which appear in README. High cosine
similarity means *"about the same subject"*, not *"one can be deleted"*. Always read the candidates
before acting on a score.

## Local RAG, if you actually need it

`examples/local-rag.sh` is a working ~40-line pattern: `freellama task --task embedding
--input-file` produces the vectors, and `jq` does cosine + top-k. **FreeLlama owns no vector
store**, deliberately — persistence is a standing non-goal, and a stale index fails *silently*,
returning confidently wrong files as the corpus drifts. You own storage and staleness; swap the
flat file for sqlite-vec or LanceDB when it outgrows one.

Measured over 242 chunks of this repo's Rust source, top-1 retrieval got 2 of 3. The failure is
worth more than the successes: *"how does it avoid loading two models into memory"* returned
`lib.rs` instead of `platform.rs`, and has now missed in two independent runs. `platform.rs` never
uses the phrase — it says `managed_execution`, `RwLock`, "admission permit". **Embeddings match how
text is phrased, and code routinely names a concept in vocabulary that looks nothing like the
question.** When you can guess the identifier, grep finds it instantly and exactly. Reach for
embeddings when you cannot.

## Finding a model you do not have yet

`search_models` is two steps and the second is not optional:

1. **Search** (`capabilities`, `query`) returns *family* names, popular-ordered. A family is **not
   pullable**. `cloudOnly` marks models that only run on Ollama's hosted service.
2. **Inspect** (`model: "<family>"`) returns each tag with its size, context window, and
   `fitsInMemory` computed against this machine — plus the largest tag that fits.

Pulling from step 1 alone means guessing the size, which is how a 143GB tag ends up looking like a
candidate on a 48GB machine.

Step 2 **fails closed**: with `freellama serve` unreachable there is no machine profile, so memory
fit cannot be computed and **no tag is recommended** — you get `recommendationUnavailable` saying
so, rather than a confident pick. This matters because the earlier fail-open version, asked with
serve down, recommended the 143GB `qwen3-vl:235b`. If you see that field, start `serve` and ask
again; do not fall back to picking the biggest tag yourself.

