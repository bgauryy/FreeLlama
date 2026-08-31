# FreeLlama MCP Server

Exposes FreeLlama's local-LLM control plane, and Ollama's full lifecycle, as
[MCP](https://modelcontextprotocol.io) tools, built on the official
[`@modelcontextprotocol/typescript-sdk`](https://github.com/modelcontextprotocol/typescript-sdk).
**Eight tools.** Four (`doctor`/`models`/`route`/`run_task`) are thin wrappers over native NAPI
bindings straight into the Rust core (`../rust-core/src/napi.rs`) — no HTTP round trip from Node to
a CLI subprocess, no reimplemented logic. Two (`ollama_manage`/`ollama_delete`) talk to Ollama's
own HTTP API for lifecycle operations the routing layer doesn't cover. One (`search_models`)
queries ollama.com. One (`delegate_research`) offloads grounded research questions to a local
model and returns a verdict computed from what the run did.

## Architecture

```
MCP client (Claude Desktop, etc.)
        │ stdio, JSON-RPC
        ▼
packages/mcp/src/index.ts     — registers 8 tools, forwards args, returns JSON as text
        │
        ├─ direct function call (native addon), for the 7 routing-plane tools:
        │       native/index.js                  — resolves + re-exports the compiled addon
        │                                          for this platform/arch (see "Platform
        │                                          support" below)
        │       native/freellama.darwin-arm64.node   — compiled from ../packages/rust-core/src/napi.rs (napi-rs)
        │       (built INTO this package, not the repo root — see "Build" below — so
        │        packages/mcp/ is self-contained and can be packed/published on its own)
        │       ├─ doctor()  → calls crate::doctor() directly, talks to Ollama, no server needed
        │       ├─ machine() / listModels() / route() / recommend() / naturalRoute()
        │       │     → one HTTP call each to a running `freellama serve` instance, decision-only
        │       │       (mirrors packages/cli/src/main.rs's own CLI implementation exactly — same pattern,
        │       │        same source of truth, nothing duplicated)
        │       └─ runTask()
        │             → routes AND executes a chat/generate/embed call in one HTTP call —
        │               the only routing-plane tool that actually does work, not just decides
        │
        ├─ direct HTTP to Ollama, for `models`' no-serve views + the 3 `ollama_*` lifecycle tools:
        │       no freellama serve involved — thin wrappers over Ollama's own API
        │       (GET /api/tags, /api/ps; POST /api/show, /api/pull, /api/generate;
        │        DELETE /api/delete)
        │
        └─ subprocess, for `delegate_research`:
                spawns benchmark/local/scripts/octocode_agent.py, which drives a local
                Ollama model equipped with the octocode CLI against an allowlisted workspace
```

## Build

```bash
# 1. Build the native addon — run from the repo root (that's where Cargo.toml lives), but the
#    compiled output is written straight into packages/mcp/native, not the repo root.
cd ..
npm install
npm run build          # cargo napi build -> packages/mcp/native/freellama.darwin-arm64.node
                        # (native/index.js + native/index.d.ts are hand-written and already
                        #  checked in — see the note in "Why native bindings" below for why)

# 2. Build this MCP server
cd mcp-server
npm install
npm run build           # tsc -> dist/index.js
```

Both steps must run at least once before `npm start`/`npm test` — `dist/index.js` imports
`native/index.js`, which `require()`s the compiled `.node` binary. The `.node` binary itself is
gitignored (it's a compiled artifact, rebuilt from `../packages/rust-core/src/napi.rs`), but `native/index.js` and
`native/index.d.ts` are checked into git since they're hand-written, not generated.

## Run

```bash
node dist/index.js
```

Communicates over stdio — this is meant to be launched by an MCP client (e.g. configured in
Claude Desktop's `claude_desktop_config.json`), not run interactively.

## Verify it actually works

`test/smoke-test.mjs` drives the server through the real MCP protocol using the SDK's own `Client`
+ `StdioClientTransport` (not a direct function call) — lists tools, calls `doctor`, calls
`doctor`'s absorbed machine profile. `test/smoke-test-protocol.mjs` asserts the machine-readable half of the tool contract:
that every tool carries behaviour annotations and an output schema, that `ollama_delete` is the
only tool flagged destructive and the read-only set is exactly what it should be, and that a
non-error result's text block and `structuredContent` are the same object. When `freellama serve`
happens to be running it goes further and validates every serve-backed output schema against a
real payload — a declared schema the real response violates is *worse* than no schema, because the
SDK turns the mismatch into a protocol error and the tool stops working, so that case is caught
here rather than in a client. `test/smoke-test-delegate.mjs` exercises `delegate_research` end to
end against this repo itself. Run all three:

```bash
npm run build
npm test
```

Expected: `tools/list` returns all 8 tool names; `doctor` returns real Ollama diagnostic JSON
(works even with no `freellama serve` running); `machine` either returns a machine profile (if
`freellama serve` is running) or a clear connection/404 error (if not) — never hangs; and the
protocol test ends with `All protocol assertions passed.`

Two more scripts exist but aren't wired into `npm test` because they're slower or destructive by
nature — run them manually when touching the lifecycle tools or the env-var defaults:

- `node test/smoke-test-lifecycle.mjs` — full round trip: lists models, checks residency, pulls a
  small real model (`qwen2.5:0.5b`), verifies it's installed, deletes it again, verifies the
  install list is back to net-zero. Requires network access and takes ~10-30s.
- `node test/env-override-check.mjs` — proves `FREELLAMA_OLLAMA_ENDPOINT` actually changes
  behavior (not just that it compiles): launches the server with the env var pointed at a dead
  port and confirms `doctor` fails to connect *there* instead of silently falling back to the
  real default.
- `node test/validate-all.mjs` — the behaviour suite: exercises **every** tool against the live
  system and asserts what each is supposed to *do*, not just that its schema is well-formed. 22
  checks covering doctor's env reporting, all four `models` views, the `minConfidence` fail-closed
  path (asserted to refuse in under 5s, i.e. before generating), embedding withholding and its
  opt-in, both `search_models` steps including that a 143GB tag is excluded, and both
  `delegate_research` verdict paths. Needs `freellama serve` and Ollama running; ~1 minute.
- `node test/smoke-test-run-task.mjs` — spawns a real `freellama serve` instance (needs port
  11435 free, so stop any `freellama serve` you already have running), then proves
  `requiredCapabilities` actually filters model selection (a `["vision"]` request succeeds and
  names a real model; a `["audio"]` request correctly fails — no audio model is installed) and
  that `run_task` really routes and executes (returns a real Ollama response, not just a
  decision). Requires the release binary built (`cargo build --release`) and takes ~15-20s.

## Tools

| Tool | Needs `freellama serve`? | What it does |
|---|---|---|
| `doctor` | Optional — Ollama half needs none; the absorbed machine profile needs serve | Ollama reachability, CLI/server version match, and the **11 memory-governing settings** (nine `OLLAMA_*` plus `LLAMA_ARG_FIT`/`LLAMA_ARG_FIT_TARGET`, which govern memory but lack the prefix an auditor greps for), each with its *effective* default. Plus chip/unified memory/CPU/disk when serve is up |
| `models` | `view: "installed"` (default) does; the other three don't | Four views of the local estate: `installed` (capabilities, VRAM, context, policy_rank), `resident` (loaded now + derived GPU/CPU split), `detail` (one model in depth, real max context), `raw` (plain `GET /api/tags`). Absorbed the former `list_models`/`ollama_ps`/`ollama_show` — see "Merging tools" |
| `route` | Yes | Deterministic model selection for a task/objective; accepts `requiredCapabilities` and `minConfidence` (forwarded to the **core** gate — the refusal names the grade, evidence, would-be model, and the two commands that raise the grade). Every decision reports its evidence dimensions (`quality_evidence`, `task_evidence`, `hardware_fit`) and a `rejected[]` list naming why each losing candidate lost |
| `search_models` | No — queries ollama.com | Search the public model library. Popular-ordered by default; flags `cloudOnly` models that cannot run locally, and cross-references what is already installed |
| `run_task` | Yes | **Routes AND executes** a chat/generate/embed call in one shot — the one routing-plane tool that does real work, not just a decision. Embedding results withhold the raw vectors by default (`returnEmbeddings: true` to get them) — see "Keeping results small" below |
| `ollama_manage` | No — direct to Ollama | `action: "pull"` downloads a model (blocks until done); `action: "stop"` force-unloads it from memory now. Both additive and idempotent |
| `ollama_delete` | No — direct to Ollama | `DELETE /api/delete`: **destructive**, permanently removes a model. Only call on an explicit human instruction naming the exact model — never on an automated staleness heuristic |
| `delegate_research` | No — spawns the research adapter directly | Hands a grounded code-research question to a local model with a citable evidence trail. Read `structuredContent`: `verification` (recommendation + why) and `citations[]` (`{step, tool, path, command}`, full and unclipped) — `summary` is the same thing as prose. Prefer `run_task` for an ordinary chat/completion/embedding call — this is the heavier-weight, specialized path. See `skills/freellama/references/task-delegation.md` for what to trust it with. |

### No annotations, no output schemas, no titles

All three were removed to shrink the per-request surface. Each removal has a real consequence,
recorded here rather than discovered later:

**Output schemas (−1,086 tok).** They bought client-side JSON-Schema validation, and that
validation was itself a hazard: a strict schema turned any undeclared upstream field into a hard
`McpError` (caught live when `/api/show` returned an undocumented `requires` field). Verified
against the SDK that `structuredContent` still reaches the client with none declared, so callers
keep the parsed object and lose only the validation. Both properties are asserted in the tests.

**Annotations (−143 tok).** This one is a genuine trade, made deliberately. The MCP spec defaults
are `readOnlyHint: false` and **`destructiveHint: true`**, so with nothing declared a client that
gates on those hints now treats *every* tool as destructive — including `doctor`. In practice few
clients gate on them today, which is what makes the trade acceptable; but it means
**`ollama_delete` no longer stands out structurally**, and its guard lives entirely in its
description. That description was rewritten to carry the weight, and a test asserts the warning
text survives.

**Titles (−65 tok).** Pure duplication of the tool name.

Result: the surface fell from **7,431 to 3,393 tokens — a 54% cut, after adding `search_models` back on top** — across these removals plus
the tool merges, `RouteDecision` de-duplication, and stripping `.describe()` from parameters whose
names already say what they are. Parameters whose *behaviour* isn't guessable from the name
(`minConfidence`, `adapter`, `returnEmbeddings`, `view`, `action`) kept theirs.

### No measurements compiled in

The server carries the *mechanism*, never one machine's results. Per-model research grades load at
runtime from `benchmark/evidence/model-evidence.json` (override with
`FREELLAMA_MCP_MODEL_EVIDENCE`); the table used to be a hardcoded `MODEL_EVIDENCE` constant, which
made a shipped binary assert someone else's benchmarks as universal fact and rot silently as models
changed. **Empty by default** — a model with no entry is unmeasured, which yields a `verify`
verdict. That is the correct answer when nothing is known, and safer than assuming strength.

Tool descriptions and instructions state the *rules* ("accuracy collapses on small models", "never
trade size for speed") without the specific figures that produced them. The figures live in
`skills/freellama/references/`, `benchmark/`, and `.octocode/evals/`, where a reader can also see
how they were measured and when they were last checked.

### Which tools earn their place

The tool list is re-sent on every request, so an unused tool is a permanent tax. Four were removed
by that standard:

- **`natural_route`** — its own description said to call `route` instead; its consumer is an LLM
  that already knows the task kind; and it broke silently when its intent model was deleted.
- **`machine`** — absorbed into `doctor`, whose data you want at the same moment.
- **`list_models` / `ollama_ps` / `ollama_show`** — merged into `models` with four views.
- **`recommend`** — superseded. Asked for a vision model it returned exactly one suggestion,
  `gemma3:4b`, from a hand-maintained catalog — a 4B model, **below the ~12B floor this project
  measured for research**. A curated list that must be updated by hand goes stale, and
  `search_models` covers the same ground from the live library with per-tag memory fit. The server
  route and CLI subcommand both remain.

Plus one merge: **`ollama_pull` + `ollama_stop` → `ollama_manage`** (both additive and idempotent).
`ollama_delete` stays separate — `action: "delete"` is one token from `action: "stop"`, and a
distinct tool name is a stronger barrier than an enum value for the only irreversible operation.

Result: **13 tools → 8**, surface **7,431 → 3,219 tokens (−57%)**.

### The token/latency balance — where the cost actually is

Three separate costs, measured, in order of how much they matter:

**1. Schema surface: ~3,393 tokens, paid on every request.** This dwarfs any single call's payload.
It was 7,431 across 13 tools. The breakdown was the surprise — **input+output schemas were 4,736
tokens against only 1,374 for all the descriptions combined**, so trimming prose was optimizing the
wrong half. Two structural fixes did the work: merging the three read-only inspection tools, and
noticing the full 14-field `RouteDecision` shape was being inlined into **four** separate output
schemas. It is now documented once on `route`; the rest carry it as a passthrough, losing nothing
at validation time. A test asserts both properties so they can't silently regress.

**2. Per-call payloads.** Already handled: embedding vectors withheld by default (~4,400 → ~400
tokens), `license`/`modelfile` withheld from `models{view:"detail"}` (97% of that payload), compact
serialization above 8 KB.

**3. The delegation decision — the biggest lever, and it is about quality, not speed.**
Measured on real source files, a delegated answer costs ~130–175 tokens regardless of how big the
input was:

| File | Read it yourself | Delegated | Reduction | Correct |
|---|---:|---:|---:|:--:|
| `packages/rust-core/src/napi.rs` | 3,255 tok | 127 tok | 96% | ✅ |
| `packages/rust-core/src/platform.rs` | 11,678 tok | 149 tok | 99% | ✅ |
| `packages/rust-core/src/lib.rs` | 8,308 tok | 173 tok | 98% | ✅ |
| **Total** | **23,241** | **449** | **98%** | **3/3** |

An earlier version of this guidance said "source under 5k tokens → read it yourself". That was
calibrated on latency using a toy file, and it is wrong on the axes that matter: a 3k-token file
still yields 96% reduction at full accuracy. **The token break-even is low — past roughly 1k tokens
of source, delegating already wins.**

What actually binds is quality:

| Situation | Do |
|---|---|
| Using a model measured strong (`qwen3.8:27b-mlx`) | **Delegate** — 96–99% reduction at full accuracy |
| Any model below ~12B | **Never delegate.** 7B scored 2/8, 3B 3/8, 0.5B 0/8. A cheap wrong answer costs more than the tokens it saved — never trade model size for speed |
| Judgment work (review, "is this good", design) | **Do it yourself** — ~67% vs 98.9%, identical confident tone. Token savings don't survive a wrong answer |
| Source <1k tokens *and* you are blocked waiting | Read it — the one case where ~10–15s loses |
| Privacy / rate-limited | Delegate regardless |

### Images — correction: vision works, routing does not

An earlier version of this section claimed there was **no working general-vision model installed**.
That was wrong, and worth recording because of *how* it was wrong: I tested the three models whose
names suggested vision (`llama3.2-vision`, `deepseek-ocr`, `gemma4:12b-mlx`) and never tested the
two large models already in daily use for text. Both do real vision.

| Model | Verified |
|---|---|
| `qwen3.8:27b-mlx` | Describes UIs and charts accurately; transcribed a terminal screenshot **spelling `freellama` correctly**, which the OCR model got wrong. ~17–37s |
| `muse-glimmer:30b-mlx` | Accurate UI description. ~45s |
| `deepseek-ocr:latest` | OCR only — **degenerates into a repeating loop** on an image without text, and drops characters |
| `llama3.2-vision:latest` | **Cannot load** on this Ollama build (`unknown model architecture: mllama`) |

**The real problem is routing, not capability.** `task: "vision"` with no configured policy ranks
on capability metadata alone and selects `deepseek-ocr:latest` — the worst of the four — with
`confidence: "low"` and `evidence: capability_metadata_only`. Exactly the failure already recorded
for code repair, now confirmed for vision.

So the guidance is the opposite of what it was: **pass `model: "qwen3.8:27b-mlx"` explicitly** with
`requiredCapabilities: ["vision"]`, or set `minConfidence: "medium"` so the weak pick is refused
rather than used.

### Finding models that are not installed yet

`search_models` browses the public Ollama library. There is **no JSON API** — `Accept:
application/json` still returns HTML, and `/api/search`, `/search.json`, and the registry
`_catalog` all 404 — so it parses the rendered pages. That couples it to Ollama's markup; a
redesign shows up as zero results rather than an exception, and the tests assert a non-empty parse
so the breakage is caught loudly.

**It is a two-step flow, and step 2 is not optional.** Search returns *family* names (`gemma4`),
and **a family is not pullable** — you pull a tag (`gemma4:12b`), and only the tag carries the size
that decides whether it fits. The step-1 response carries a `nextStep` field saying exactly that.

**Step 1 — search** (`capabilities`, `query`, `order`):

- **Popular by default.** Verified: omitting `o` returns rankings identical to `o=popular`, and
  `o=newest` differs. `newest` exists but is rarely wanted — a new model has no track record.
- Site rank is **not** pull count — a 26K-pull model can outrank a 1.1M one. Judge with `pulls`.
- **`cloudOnly`** marks models that run on Ollama's *hosted* service and cannot run locally. Two of
  the top six vision results are cloud-only.
- **`installed`** cross-references what you already have.

**Step 2 — inspect** (`model: "<family>"`): every tag with size, context window, modalities, plus
`fitsInMemory` computed against this machine's real `unified_memory_bytes`, and a `recommendation`.

Live example on a 52 GB machine (budget 31 GB):

```
qwen3-vl:latest  6.1GB  ctx=256K  fits=true      gemma4:26b   19GB  fits=true
qwen3-vl:30b     20GB   ctx=256K  fits=true      gemma4:31b   20GB  fits=true
qwen3-vl:235b   143GB   ctx=256K  fits=FALSE  <- correctly excluded
-> recommends qwen3-vl:32b (21GB)
```

The fit budget is **60% of total memory**, not 100%: a model also needs room for its KV cache and
for anything else resident, and this machine has already crashed by co-residenting two large
models. The recommendation prefers the **largest** tag that fits, because research accuracy
collapses below ~12B — and it ships with configuration guidance (send an explicit `num_ctx` rather
than inheriting Ollama's VRAM-tiered default, which reaches 256K here; use `keepAlive: "0"` for
one-offs) and a caution that this repo measured a **GGUF build beating its `-mlx` counterpart** for
the same family, so packaging must be benchmarked rather than inferred from a suffix.

**Why one tool and not a separate `model_info`.** Both modes are read-only with identical
behaviour, and the flow is strictly sequential — you cannot usefully inspect without having
searched. One tool covering both costs **447 tokens**; two would cost roughly **700**, since the
second repeats its name, description, and the shared endpoint/limit params. The same
`models`-with-views precedent applies. The per-request budget guard was raised 3,400 → 3,550 to
accommodate the flow, with the reason recorded in the test rather than silently bumped.

### Merging tools

Two merges happened, under one rule: **merge only tools whose behaviour profile is identical.**

- `list_models` + `ollama_ps` + `ollama_show` → **`models`** with four views. All three were
  read-only, so nothing was lost.
- `ollama_pull` + `ollama_stop` → **`ollama_manage`** with an `action` discriminator. Both are
  additive and idempotent — neither removes an installed model, and repeating either is a no-op.
  540 → 397 tokens.

**`ollama_delete` stays separate**, and the reason is narrower than it first looks. An earlier
version of this section claimed a merged tool would have to "lie" about delete — that was wrong:
the spec says `destructiveHint` means a tool *may* perform destructive updates, so marking a
delete-capable tool `true` would be accurate. The real reasons are:

1. **Affordance.** `action: "delete"` is one token away from `action: "stop"` in a generated call;
   a distinct tool *name* is a stronger barrier against that slip than an enum value.
2. **Blast radius.** It is the only irreversible operation in the server, and this repo's policy is
   that it runs only when a human names the exact model.

Note the annotations that argument originally rested on were later removed at the user's request,
so `ollama_delete`'s guard is now its description alone — see "No annotations, no output schemas".

### Escalation: refusing instead of answering weakly

Two measured problems, one shape of fix — surface the doubt as a machine-readable signal and let
the orchestrator decide. Nothing here escalates on its own, matching how the rest of this server
treats decisions that change state.

**1. `minConfidence` on `route` / `run_task`.** The server already grades every decision
(`route_evidence` in `packages/rust-core/src/platform.rs`): `medium` only when the task has *both* a configured policy
and benchmark data, `low` otherwise — there is no `high`. A `low` / `capability_metadata_only`
decision is exactly what returned `qwen2.5:0.5b` for code repair on this machine, and unchecked it
comes back looking like any other answer. Passing `minConfidence: "medium"` turns that into a
fail-closed refusal naming the model it would have picked and the evidence that was missing:

```
Route refused: confidence is "low" (evidence: capability_metadata_only), below the requested
minimum "medium". Selected model would have been "deepseek-ocr:latest" for reasons
[installed, capabilities_satisfied, capability_only_fallback].
```

For `run_task` the check runs as a `route` preflight, so it refuses **before** any tokens are
spent — measured at 1ms. Gating the result afterwards would save nothing. The extra round trip
only happens when the option is set.

**2. A `verification` block on every `delegate_research` answer.** The local model is 98.9%
accurate on grounded lookups and ~67% on judgment calls, **in the same confident tone** — so the
answer text carries no signal about which one you got. Every result now carries `accept` /
`verify` / `escalate`, derived from what the run actually did, never from the model's self-report:

| Verdict | Trigger | Verified |
|---|---|---|
| `escalate` | Zero tool calls — parametric recall, not research | 0 calls in 6.4s |
| `verify` | Judgment-shaped question (labelled keyword heuristic), or >5 tool calls (outside the measured 1–5 file envelope) | 6 calls in 34.0s |
| `accept` | Grounded, lookup-shaped, within envelope | 2 calls in 12.1s |

### How small can the local model be? (measured 2026-08-30)

8 grounded single-file lookups, ground truth `grep`-verified, `bash` adapter, each model unloaded
before the next. Full write-up: `.octocode/evals/2026-08-30-small-model-deep-research-eval.md`.

| Model | Size | Solved | Median wall | Median local input tok | Hard errors |
|---|---|---:|---:|---:|---:|
| `qwen2.5:0.5b` | 0.5B | **0/8** | 2.0s | 8,959 | 4 |
| `llama3.2:3b` | 3B | 3/8 | 0.8s | 551 | 0 |
| `qwen2.5:7b` | 7B | 2/8 | 2.0s | 587 | 0 |
| `gemma4:12b-mlx` | 12B | 6/8 | 7.0s | 1,341 | 0 |
| `qwen3.8:27b-mlx` | 27B | **8/8** | 11.8s | 1,350 | 0 |

**Research falls off a cliff below ~12B; 27B is the only reliable size.** The 12B's 6/8 is on the
easiest possible question shape — the same model scores 6.7% on the 30-question multi-repo suite
and 0/10 on the real-repository benchmark. At 0.5B the model was both wrong and *expensive*,
burning 8,959 local input tokens flailing to its turn ceiling, and answering "the exact name of the
public function is `packages/rust-core/src/lib.rs`". Note also that accuracy is **not monotonic** at the small end
(7B scored below 3B) — at n=8 both are just noise around "does not work".

Small models are 15x faster and that is worthless: *fast error exits are not speed wins.*

**This eval changed the product.** It was run to validate the `verification` verdict, and found it
was **model-blind** — quoting `qwen3.8:27b-mlx`'s 98.9% base rate no matter which model answered,
so a 3B model's grounded-but-wrong answer came back as `accept` wearing someone else's accuracy.
Pooled, `verify` was a perfect negative predictor (0% correct, n=5) but `accept` was only 61%.
`assessDelegatedAnswer` now gates on a `MODEL_EVIDENCE` table: models measured unusable return
`escalate` regardless of grounding, `gemma4:12b-mlx` returns `verify`, and an **unmeasured** model
returns `verify` stating that no base rate applies. A regression test pins the 3B case.

### Choosing the research adapter

`delegate_research` runs one of two interchangeable adapters — identical env interface, identical
result shape, so the choice is pure routing. This repo's own benchmark
(`benchmark/local/results/*/aggregate.json`, 30 questions x 3 repos, one variable) settles it:

| Model | bash pass@1 | octocode pass@1 | bash median | octocode median |
|---|---|---|---|---|
| `qwen3.8:27b-mlx` | 86.7% | 86.7% | **19.6s** | 55.6s |
| `muse-glimmer:30b-mlx` | **96.7%** | 63.3% | **28.3s** | 103.0s |
| `gemma4:12b-mlx` | 6.7% | 0.0% | — | — |

bash wins or ties on every model, at **116.5 vs 53.8 successful tasks/hour**. Confirmed live on a
single question: **15.7s / 791 input tokens** (bash) vs **~40s / 7,761** (octocode) — 9.8x cheaper.
`bash` is therefore the default (`FREELLAMA_MCP_DEFAULT_ADAPTER` to change it); `octocode` stays
available for questions that suit structured search, but has to be asked for.

**The default *model* is deliberately unchanged.** muse-glimmer's 96.7% above is tempting, but
`.octocode/evals/2026-08-24-real-repository-agent-benchmark.md` ranks `qwen3.8:27b-mlx` first
(7/10) over muse-glimmer (6/10) on real-repository work, noting muse is "the best narrow bug fixer"
that "failed the explanation-heavy and complex tasks". The two benchmarks measure different task
shapes; neither supersedes the other, so the safer-for-breadth default stands and the trade-off is
documented on the `model` parameter instead.

### Timeouts, and what "won't hang" actually required

`reqwest::Client` applies **no request timeout by default**. Only a refused connection failed
fast; against an endpoint that accepted the TCP connection and then never answered, every
serve-backed tool hung indefinitely — verified with a black-hole listener, `machine` was still
pending at 45s, directly contradicting this README's own claim that these calls "return a clear
connection error, they won't hang". `packages/rust-core/src/napi.rs` now applies a per-request timeout, split by what
the request is actually doing:

| Path | Timeout | Env override | Why |
|---|---|---|---|
| `models` / `route` | 30s | `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | Pure computation over an in-memory model list; seconds means wedged, not busy |
| `run_task` (and `naturalRoute` in the native binding) | 900s | `FREELLAMA_TASK_TIMEOUT_SECONDS` | A model actually generates. Ollama's own `OLLAMA_LOAD_TIMEOUT` is 5m *before it gives up on the load alone*, so a short budget here aborts work that was going to succeed |
| `doctor` | 30s | — | It is the tool you reach for when something else is already timing out |

Same measurement after the fix: a clean `isError` at 30.0s instead of an unbounded hang.

The server also no longer dies on a client disconnect. A client that vanishes mid-response leaves
the stdio transport writing to a closed pipe, and the resulting unhandled `EPIPE` killed the
process with a stack trace on stderr (reproduced, and confirmed against a guard-free control
build: exit code 1 with an `Unhandled 'error' event` trace, versus exit code 0 and a clean stderr
with the guard). It now exits quietly through the normal path, so the subprocess cleanup still
runs and a real error isn't buried under an EPIPE trace.

### Ollama configuration this server knows about

`doctor` reports eleven memory-governing settings — nine `OLLAMA_*` plus `LLAMA_ARG_FIT` and
`LLAMA_ARG_FIT_TARGET` — each with its **effective** default, because
`launchctl getenv` returning empty means "Ollama picks", which is not the same as "off". That
distinction is not academic: an earlier version of the `OLLAMA_MAX_LOADED_MODELS` advisory read
the `0` in Ollama's `envconfig/config.go` and reported the default as *unlimited*. The `0` is a
sentinel; `server/sched.go` resolves it to `defaultModelsPerGPU * gpu_count` = **3** on a
single-GPU machine. The advisory still stands — 3 x ~22GB does not fit in 48GB — but its stated
reason was wrong, and it has been corrected here, in `packages/rust-core/src/lib.rs`, and in
`skills/freellama/references/model-selection.md`.

The three that move the most memory, none of which were reported before:

- **`OLLAMA_CONTEXT_LENGTH`** — VRAM-tiered (4k under 24GiB, 32k for 24–48GiB, **256k at 48GiB+**).
  FreeLlama's routing always sends an explicit `num_ctx`, so anything through `serve` is insulated;
  a direct Ollama call on a 48GB machine is not.
- **`OLLAMA_NUM_PARALLEL`** — memory scales by `NUM_PARALLEL x context_length`. It multiplies
  KV-cache memory, it does not merely add scheduling slots.
- **`OLLAMA_KV_CACHE_TYPE`** — `f16` by default; `q8_0` roughly halves KV-cache memory (needs
  `OLLAMA_FLASH_ATTENTION`).

`models{view:"resident"}` additionally derives the GPU/CPU split Ollama's own context-length doc tells you to
check, and warns on partial CPU offload — the quietest failure mode here, since the model still
answers, just many times slower, with no error anywhere.

### Platform support

The native addon is a compiled artifact named by target triple
(`freellama.<platform>-<arch>[-<abi>].node`), so `native/index.js` derives the candidate names the
same way napi-rs does rather than hardcoding one. On an unsupported platform it fails with a
message naming exactly what it looked for and how to build it, instead of a bare
`MODULE_NOT_FOUND` from deep inside the server:

```
freellama: no native addon for linux-x64.
Looked for: freellama.linux-x64-gnu.node, freellama.linux-x64-musl.node, freellama.linux-x64.node in .../native
```

`os`/`cpu` restrictions were **removed**: they made npm refuse the install outright on Linux, including for someone intending to build the addon from source, which is fully supported. The loader already fails with an actionable message naming the exact filenames it looked for, so a hard install-time block cost more than it bought. `engines` still requires Node >= 20, and `npm run build:native` builds the addon anywhere a Rust toolchain exists. Only the arm64 macOS binary is currently built and shipped; building for
`x86_64-apple-darwin` needs nothing but running the build on (or cross-compiling to) that target —
it is already listed in the repo-root `package.json`'s `napi.targets`. Other platforms need a
Rust toolchain and a local `npm run build` from the repo root.

`doctor`/`ollama_*`/`models` (in its no-serve views) accept an optional `ollamaEndpoint` argument (default
`http://127.0.0.1:11434`, raw Ollama). `models`/`route`
accept an optional `endpoint` argument (default `http://127.0.0.1:11435`, the FreeLlama proxy).
Start the server the latter group depends on with `cargo run --release -- serve
--recommendation-catalog recommendations.example.toml` from the repo root — see
`skills/freellama/references/proxy-vs-serve.md` for the `proxy` vs `serve` distinction (only
`serve` has the `/_freellama/v1/*` routes these tools call).

## Configuration (environment variables)

Every default in this server is overridable via an environment variable — none require editing
source or recompiling. Set these on the MCP client's server-launch config (e.g. `.mcp.json`'s
`env` block) or in the shell that starts `node dist/index.js`.

| Variable | Default | Affects |
|---|---|---|
| `FREELLAMA_OLLAMA_ENDPOINT` | `http://127.0.0.1:11434` | `doctor`'s default `ollamaEndpoint`; also the default base URL for all `ollama_*` lifecycle tools |
| `FREELLAMA_SERVE_ENDPOINT` | `http://127.0.0.1:11435` | Default `endpoint` for `doctor`'s machine profile, `models`/`route`/`recommend`, and the endpoint `delegate_research` hands to the local model (same name used by `packages/rust-core/src/napi.rs` on the Rust side — one name, one meaning, across both languages) |
| `FREELLAMA_MCP_DEFAULT_MODEL` | `qwen3.8:27b-mlx` | `delegate_research`'s default `model` when the caller doesn't specify one |
| `FREELLAMA_MCP_MAX_TURNS` | `8` | Max agent turns `delegate_research` allows the local model before giving up |
| `FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS` | `180` | Wall-clock timeout for the whole `delegate_research` subprocess |
| `FREELLAMA_MCP_PULL_TIMEOUT_SECONDS` | `1200` | Default timeout for `ollama_manage` action `"pull"` (overridable per-call via `timeoutSeconds`) |
| `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS` | `30` | Default timeout for every other direct Ollama HTTP call (`models` no-serve views, `ollama_stop`, `ollama_delete`) |
| `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | `30` | Request timeout for the decision-only serve calls (`machine`/`models`/`route`/`recommend`) — read by `packages/rust-core/src/napi.rs` |
| `FREELLAMA_TASK_TIMEOUT_SECONDS` | `900` | Request timeout for the calls that make a model generate (`run_task`, and `naturalRoute` in the native binding) — read by `packages/rust-core/src/napi.rs` |
| `FREELLAMA_MCP_DEFAULT_ADAPTER` | `bash` | `delegate_research`'s adapter when the caller doesn't pass one. `octocode` to restore the old behaviour — see "Choosing the research adapter" for why it is no longer the default |
| `FREELLAMA_MCP_ALLOWED_ROOTS` | this repo | Colon-separated allowlist of directories `delegate_research` is permitted to read from — see the security note below |

The Rust/native-addon layer (`packages/rust-core/src/napi.rs`) additionally reads `FREELLAMA_OLLAMA_ENDPOINT` and
`FREELLAMA_SERVE_ENDPOINT` directly for its own defaults, using the exact same names and meanings
as above — setting them once affects both layers consistently.

## Publish

`packages/mcp/` is a self-contained package: the compiled native addon lives inside it
(`native/freellama.darwin-arm64.node`), not at the repo root, so it can be packed or published
without needing the rest of the monorepo alongside it.

```bash
# from the repo root: rebuild the native addon fresh, then this package
npm run build && npm --prefix mcp-server run build

# from packages/mcp/: confirm exactly what a publish would ship, without publishing anything
cd mcp-server
npm pack --dry-run
```

`package.json`'s `files` field (`dist`, `native`, `README.md`) is the allowlist `npm pack`/`npm
publish` uses — it ships the compiled `.node` binary and the hand-written `native/index.js`/
`native/index.d.ts` even though `*.node` is gitignored (the `files` field, not `.gitignore`,
decides what goes in the npm tarball). TypeScript source (`src/`), tests (`test/`), and
`node_modules` are excluded. `prepublishOnly` reruns both builds automatically so `npm publish`
can never ship a stale addon or a stale `dist/`. The `os`/`cpu`/`engines` fields (see "Platform
support") are what stop npm installing this on a platform the shipped binary can't serve.

This project has never actually been published to a registry — treat `npm publish` as a real,
irreversible, public action and confirm with whoever owns this repo before running it for real.

## Why native bindings instead of shelling out to the CLI or hitting the HTTP API from Node

- **No reimplementation**: the Rust side of every non-`doctor` tool is one HTTP call to the
  already-running server, exactly mirroring `packages/cli/src/main.rs`'s own CLI implementation
  (`print_get`/`print_post`/`request_route`). There is exactly one place routing/recommendation/
  model-discovery logic lives — the running server — whether you're using the CLI, curl, or this
  MCP server.
- **One connection pool, not one per call**: `packages/rust-core/src/napi.rs` holds a single `reqwest::Client` in a
  `OnceLock` and clones the handle per request. A `Client` owns a connection pool, a DNS resolver,
  and background driver tasks; building a fresh one per tool call (which this module used to do)
  discards all of that and reconnects from scratch every time. This now matches the server side,
  which already kept a single client in `PlatformState`.
- **`unsafe_code` stays isolated**: the whole crate is `deny(unsafe_code)` except `packages/rust-core/src/napi.rs`,
  which carries an explicit `#[allow(unsafe_code)]` — napi-derive's generated FFI glue needs it,
  and it's the only place that does.
- **The napi build is feature-gated off by default** (`cargo build` never touches it) because
  napi's FFI symbols only resolve inside a loaded Node process — linking them into the standalone
  `freellama` binary fails. Building the addon means
  `cargo build --release --no-default-features --features napi` (see the root `Cargo.toml`'s
  `[[bin]] required-features = ["cli"]`, which is what actually excludes the bin target).
