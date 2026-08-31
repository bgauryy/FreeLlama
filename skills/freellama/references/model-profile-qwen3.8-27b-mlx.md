# Capability profile: `qwen3.8:27b-mlx`

Load before trusting this model's output unsupervised, or before building a profile like this one
for a different model.

Evidence-based, not vendor-claimed. Every finding below was independently verified against ground
truth this session controlled (real code, real math, a real 30-question benchmark) — not the
model's own confidence. Methodology: `octocode-graph-eval` (anchor requirement, verifier
independence — every claim checked in a context separate from the one that produced it).

## Summary verdict

**Strong**: grounded code search/retrieval, long-context fact-finding, honesty under a false
premise, strict instruction-following, JSON tool-calling reliability (with caveats — see Tool Use).
**Weak**: unsupervised code review (real false-positive rate), multi-step math/logic *unless
`think:true`* — with `think:false` it guesses instantly and wrong.
**Sharp edge**: `think:false` with a small `num_predict` can silently return an *empty* answer if the
model tries to think anyway and exhausts the budget before emitting content — always pass
`"think": false` explicitly, never rely on a default.

## Field-by-field results

### 1. Grounded code search / research offloading — ✅ Excellent
Asked it to find every `reqwest::Client::builder()` construction in `src/` and report timeout
status per one, via the octocode tool. **Result: 100% accurate, all 4 file:line citations correct**,
independently verified with `grep` — including correctly spotting the one client missing a timeout.
Cost: 1 tool call, 2,035 input / 256 output tokens. **This is the killer use case: cheap, fast,
verifiable-by-construction (it cites paths you can check).**

### 2. Long-context retrieval — ✅ Strong
Given the full `platform` module (11,499 prompt tokens, now split across [`packages/rust-core/src/platform/`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/platform)) and asked to find a specific special-cased
model/task/num_ctx combination buried in the middle. **Correct model tag, correct num_ctx value,
correct containing function, verbatim code citation** — 7.4s to answer. Minor terminology looseness
(called the containing function's name when the precise identifier was a local variable inside it)
but no factual error.

### 3. Honesty under a false premise — ✅ Strong
Asked about a fabricated function (`calculate_quantum_entanglement_score()`) that doesn't exist.
**Correctly refused to fabricate**, explicitly named the false premise, did not invent plausible-
sounding details. No hallucination under direct pressure to do so.

### 4. Strict instruction-following — ✅ Strong
Asked for the exact string `ACKNOWLEDGED` with nothing else. **Exact match, zero extra tokens,
zero punctuation.**

### 5. Multi-step math/logic — ⚠️ Conditional on `think`
Same word problem (correct answer: 8), two conditions:
- `think:false`: answered **"10" — wrong — in 2 tokens, 0.2s.** No visible work; it pattern-matched
  a guess rather than solving anything.
- `think:true`: answered **"8" — correct**, full step-by-step algebra shown, 217 tokens, 6.6s.

**Actionable rule: never trust a `think:false` answer to a task that requires actual multi-step
reasoning. Reserve `think:false` for tool-orchestration/retrieval tasks where the "thinking" would
just be overhead, not the actual work product.**

### 6. Unsupervised code review — ❌ Unreliable, verify everything
Given the real `send_with_retries` retry function from [`proxy.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/proxy.rs) and asked to find bugs.
Produced 6 findings; checked each against the actual code and the `bytes` crate's real semantics:

| Claim | Verdict |
|---|---|
| "Infinite loop on non-retryable errors" | **Hallucinated.** Traced the match arms — exhaustive, no infinite loop exists. |
| Jitter isn't cryptographically random | Correct, mildly overstated severity. |
| Exponential term could overflow / no delay cap | Technically true, practically moot (`MAX_ATTEMPTS=3` bounds it). |
| Retries all error types indiscriminately | Legitimate, fair — real design gap (low impact for a localhost-only proxy). |
| "No timeout on retry loop" | Wrong for the real file (a timeout exists elsewhere) — but the snippet given didn't include that line, so this is context-limited, not fabricated. |
| `body.clone()` is "inefficient" for large bodies | **Factually wrong** — `bytes::Bytes::clone()` is a cheap refcount bump, not a data copy. Real knowledge gap. |

**2 of 6 findings were wrong** (one hallucinated, one factually incorrect), one was a reasonable
inference from an incomplete snippet, three were legitimate with varying real-world value.
**A blind trust-the-review workflow would act on a fake bug about a third of the time.**

### 7. Structured JSON tool-calling & agentic tool use — ✅ Good, with real cost tradeoffs
From the formal 30-question benchmark (see [`benchmark/local/`](https://github.com/bgauryy/FreeLlama/blob/main/benchmark/local)), comparing the same model with a
purpose-built tool (octocode) vs. raw bash:

| Agent | Pass rate | Median tool calls | Median tokens in/out | Median time |
|---|---|---|---|---|
| octocode | 87% (26/30) | 4 | 8,689 / 538 | 55.6s |
| bash | 87% (26/30) | 3 | 1,845 / 192 | 19.6s |

**Same accuracy either way — bash was ~2.8x faster and used ~4.7x fewer input tokens on this
suite.** The octocode tool didn't buy qwen anything in correctness here; it cost time and tokens.
(The other two models in the same harness showed the opposite risk — they could not reliably emit
the octocode tool-call JSON and did dramatically better on plain bash. Note this is a statement
about the *octocode condition only*: `muse-glimmer:30b-mlx` scored **96.7% on bash**, the highest
of any model on that suite, so it is not a weak model — it is a model that struggles with that
particular tool schema. `gemma4:12b-mlx` was weak everywhere: 6.7% on bash, 0% on octocode. qwen is
the one model where the tool choice is a genuine tradeoff rather than a clear loss.)

## Which `npx octocode` tools actually get used (real usage data, 90+ trials, 3 models)

| Tool | Calls | Success rate | Notes |
|---|---|---|---|
| `localGetFileContent` | 168 | 100% | The workhorse — reading files/regions dominates every trial. |
| `localSearchCode` | 76 | 100% | Second most common — finding candidate locations. |
| `localFindFiles` | 48 | 100% | Orientation — "what files exist." |
| `localViewStructure` | 44 | 100% | Orientation — "what's the directory shape." |
| `lspGetSemantics` | **2** | 100% (when used) | **Almost never reached for**, despite being available every trial. |

**Why `lspGetSemantics` is barely used**: it requires a `lineHint` obtained from a prior
search/`documentSymbols` call — a two-step protocol — while the other four tools work standalone in
one call. Models default to the simpler text-search-then-read pattern and rarely graduate to actual
semantic queries, even on tasks (like tracing a call chain or finding all references) where LSP
would give a more precise, cheaper answer than repeated text search. **If you need call-hierarchy or
reference-tracing accuracy specifically, don't assume the model will reach for `lspGetSemantics` on
its own — the system prompt may need to push harder, or the task may need to explicitly suggest it.**

## Practical usage recommendations

1. **Offload code research/search freely.** This is qwen's strongest, cheapest, most verifiable
   mode — answers cite checkable file:line evidence, verified 100% accurate on a real multi-file
   codebase question.
2. **Never skip verification on code review or bug-finding output.** ~1/3 false-positive rate
   observed on real code; a fabricated "bug" was stated with the same confident tone as the real
   findings — confidence is not a signal of correctness for this model.
3. **Set `think:true` for anything requiring actual multi-step reasoning** (math, multi-constraint
   logic); **set `think:false` explicitly for tool-orchestration/retrieval tasks** where you want
   speed and the "thinking" would just be overhead — and always pass `think` explicitly, since an
   implicit default can silently eat your entire token budget and return nothing.
4. **For simple, well-defined research questions, plain bash may be as good and 2-3x cheaper**
   than octocode for this model specifically — the tool is not automatically worth its overhead. Use
   octocode when task complexity actually needs structural/semantic tools (AST search, LSP), not for
   the same grep-and-cat work a shell can do directly.
5. **Don't expect `lspGetSemantics` to get used without help** — if a task genuinely needs semantic
   precision (call hierarchies, exact references), the prompt likely needs to name the tool
   explicitly rather than trusting the model to escalate to it on its own.

## 100-challenge scaled interview (auto-graded, then manually re-verified)

Ran 100 challenges across 10 categories (math, logic, instructions, honesty, knowledge, text,
code-tracing, format compliance, trick questions, `think` on/off comparison), each with a
programmatic checker, then **manually re-derived ground truth for every one of the 6 auto-graded
failures** before trusting the score — this step mattered:

- **Raw auto-grade: 94/100.**
- **2 of the 6 "failures" were my grader's fault**, not the model's: one had a wrong hand-computed
  expected value (vowel count), one used too-strict string matching against a correct but
  differently-phrased answer. **Corrected: 96/100.**
- **3 confirmed real errors**, all multi-step arithmetic-under-pressure: a palindrome check gotten
  backwards, and two money/age word problems where a "re-split" or reference-frame twist caused a
  dropped step even once with `think:true`.
- **1 confirmed real limitation, distinct from a wrong answer**: on the hardest age puzzle, `think:true`
  with `num_predict:500` **ran out of its thinking budget and returned nothing** — the same class of
  bug as the plain `think:false`-with-small-budget empty-response gotcha above, but happening even
  with reasoning enabled once the problem is hard enough. Cap `num_predict` generously (800+) for
  genuinely hard reasoning, not just the 300-500 that was fine for easier puzzles.

**The actual lesson, independent of the score:** a fully deterministic, hand-authored auto-grader
still had a 2% error rate of its own. Scale (100 challenges) doesn't substitute for verifying the
verifier — every reported failure needs its ground truth re-derived by hand before it counts, or
the "eval" is just a second unverified opinion, not a check. Full per-question data:
`/tmp/qwen_100_challenges_results.json` (from this session; not committed — regenerate to reproduce).

## 100-question real-repo offload test (auto-generated ground truth, manually re-verified)

Auto-generated 100 "where is `<symbol>` defined" questions directly from real symbols in the pinned
click/zustand/openui repos (ground truth extracted by regex from source, not hand-picked), then
offloaded every one to `qwen3.8:27b-mlx` through the real production adapter
(`octocode_agent.py`) — this is the actual offloading path, not a synthetic test.

**Raw auto-grade: 78/100.** Manually verified every one of the 22 failures before trusting that
number — the same discipline as the 100-challenge run, and it mattered even more here:

| Category | Count | Finding |
|---|---|---|
| Grader too strict | 9 | **Model was correct.** `octocode` resolves paths relative to a detected sub-project root (e.g. the sub-project's own `package.json`), so it reported `packages/lang-core/src/library.ts` instead of `openui/packages/lang-core/src/library.ts` — same file, verified by construction, my exact-string grader was wrong. |
| Infra: HTTP 500, retries exhausted | 11 | Real infra failures — exhausted both the proxy's 3 retries and the adapter's 2 extra retries. Nearly identical stats across all 11 (~622 tokens, 1 tool call, ~24-28s) — points to a *sustained* degraded window, not isolated blips. |
| Infra: hard timeout | 1 | `zustand-immer` — 0 tool calls, 0 tokens, full 180s timeout, complete non-response. |
| Genuine model error | 1 | `click-echo` — answered `termui.py`, real answer is `utils.py`. Confused `echo` with the similarly-named `echo_via_pager` (which *is* in `termui.py`) — a real but understandable mistake. |

**Corrected reasoning accuracy: 87/88 = 98.9%** on trials that actually got a response — only one
real mistake in 88 answerable questions, and it's an understandable name-confusion, not a
comprehension failure.

**Corrected infra reliability: 12/100 trials (12%) hit total failure** — noticeably worse than the
~5-8% observed earlier in this session on shorter runs. This points to Ollama's health degrading
over many hours of continuous inference across a long session, not a one-off blip. **The bottleneck
at scale is infrastructure endurance, not model competence** — retry+backoff+timeout (already built
into the proxy) rides out brief hiccups but isn't sufficient for a sustained degraded window; a
production deployment running this pattern for hours should budget for periodic Ollama restarts or
a more aggressive retry ceiling, not assume the existing 3+2 retry budget is enough indefinitely.

**Practical implication for multi-repo workspaces**: if you're grading or otherwise exact-matching
file paths from octocode/agent output in a workspace containing multiple sub-projects (each with its
own `package.json`/project root), expect paths reported relative to the *nearest* detected project
root, not the outer workspace root — don't grade with a strict full-path prefix match; check that
the reported relative path resolves to a real, unique file instead.

## Sources / evidence trail

All findings above are reproducible: challenges 1-6 were run live against `qwen3.8:27b-mlx` through
`http://127.0.0.1:11435/api/chat` (the FreeLlama proxy) this session; challenge 7 and the tool-usage
table are computed directly from the harness's per-trial JSON output — re-run
`python3` over those files to reproduce the counts.

Next: see `references/task-delegation.md` for how these findings translate into a delegate/verify
decision.
