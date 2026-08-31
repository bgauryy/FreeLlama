# Picking and configuring models smartly

Load when picking a model for a task, co-residenting models, or before running `bench-all` or
`route`. (There is no `recommend` MCP tool — it exists over HTTP and in the CLI only; the
agent-facing equivalent is `search_models`.)

## Where "smart" configuration actually lives

- `packages/rust-core/src/model_bench.rs::benchmark_plan()` — tiers `num_ctx` by model size (8192 for ≥16GB models,
  16384 for ≥2GB, 32768 for small models), capped by the model's own advertised context.
  `BenchmarkConfiguration` defaults: `num_predict=128`, `temperature=0`, `seed=42`, `think=false`,
  `keep_alive="5m"`.
- `packages/rust-core/src/platform.rs::profile()` — per-task-type tuning (e.g. Browser→64 tokens, Tools→256,
  CodeRepair→2048, Embedding→0), plus a `qwen_repair_profile` special-case pinning `num_ctx=8192`
  for `qwen3.8:27b-mlx` on code-repair specifically.
- `packages/rust-core/src/platform.rs`'s `managed_execution: Arc<RwLock<()>>` — resident-model tasks take a shared read
  lock (can run concurrently), a model-swap (nonresident) task takes an exclusive write lock, so
  model transitions don't race each other.

Check what's actually resident and what the router sees: the `models` (`view: "resident"` / `"installed"`) MCP tools
if an agent is connected, or `curl $FREELLAMA_ENDPOINT/_freellama/v1/models` (requires `serve`, not
bare `proxy`) / `cargo run --release -- models --endpoint $FREELLAMA_ENDPOINT` without one.

## Running several models at once (verified on this machine)

Ollama can keep multiple models resident simultaneously — governed by `OLLAMA_MAX_LOADED_MODELS`
(concurrent models in memory, unset here = Ollama's own default) and per-request `keep_alive`
(how long an idle model stays loaded before auto-unloading, default 5m, overridable per-call —
this repo's benchmark adapters set it to `"5m"` explicitly). No env var override is currently set
on this machine (`launchctl getenv OLLAMA_MAX_LOADED_MODELS` returns empty), so it runs on
Ollama's built-in default (observed on 0.32.15; the running server is now 0.33.2 — re-check with `doctor`, which reports OLLAMA_CONTEXT_LENGTH's effective default).

**Turning a model off deliberately**: `ollama stop <model>` unloads it immediately rather than
waiting for `keep_alive` to expire — useful before loading a second large model if you want to be
certain of freed memory rather than trusting the idle timer.

**Coexistence math still applies** — see the memory-contention trap below. A small model (a
vision model at ~7.8GB, an embedding model at ~300MB) coexisting with one large model (~18-30GB) is
comfortably safe on a 48GB machine; two large models is the dangerous combination that actually
crashed a run earlier in this project's history.

### Vision models: measured, not assumed

**Both large models handle images well — verified 2026-08-30, not inferred from tags.**

| Model | UI / chart description | Transcription |
|---|---|---|
| `qwen3.8:27b-mlx` | Accurate (header, two-column layout, bar chart, colours), ~37s | Accurate, ~17s — **spelled `freellama` correctly** |
| `muse-glimmer:30b-mlx` | Accurate, ~45s | Accurate, ~39s | *(measured, no longer installed)* |

Prefer `qwen3.8:27b-mlx`: faster and at least as accurate on both.

**How this was got wrong the first time, because the mistake is instructive.** An earlier version
of this section said these two were "untested, don't assume either actually handles images", and a
later audit concluded there was *no working vision model installed at all*. Both were wrong for the
same reason: only the models whose **names** suggested vision were tested (`llama3.2-vision`,
`deepseek-ocr`, `gemma4:12b-mlx`), and the two large models already in daily use for text were
never tried. Capability tags were treated as the thing to verify, when the thing to verify was the
behaviour. **Send a real image before believing any claim about vision, including this one.**

Three models were removed after testing, and the reasons are worth keeping:

- `llama3.2-vision:latest` — **could not load at all**: `unknown model architecture: 'mllama'`,
  reproducible in Ollama's own server log. Not a memory or config issue; the GGUF declares an
  `mllama.*` architecture the running `llama-server` does not recognise. Consistent with the
  CLI/server version drift `doctor` flags (Homebrew CLI vs the Ollama app's server).
- `deepseek-ocr:latest` — did OCR fast (~7s) but **dropped a character** (`freellama` → `freelama`)
  and **degenerated into a repeating loop** on an image with no text, leaking chat-template tokens.
  Fast and wrong is not a speed win.
- `gemma4:12b-mlx` — **rejected image input outright** (`this model does not support image input`)
  despite the family being multimodal; the MLX conversion dropped the vision head.

**Getting an image through FreeLlama's own routing (not just raw Ollama) is now possible**: the
`run_task` MCP tool's `images` parameter (base64, no data-URI prefix) attaches to the `prompt`
message and is forwarded verbatim to Ollama — verified live, same test image, through the full
MCP → `run_task` → `/_freellama/v1/tasks` → Ollama chain, correctly identified. Before this, the
only way to send an image through this project was a raw `/api/generate` call that bypassed
FreeLlama's routing entirely; `route`'s `requiredCapabilities: ["vision"]` picked a
capable model correctly, but there was no way to actually hand it an image until `images` was
added to `run_task`.

The `llama3.2-vision` load failure above has a standard remedy. **Fix**:
`brew upgrade ollama` (or otherwise bring the CLI in line with the running server), then re-pull
the model if it still fails. **Don't assume an installed model works — `doctor()` catches the
version mismatch, but only actually invoking the model catches an architecture-support gap like
this one.**

## The memory-contention trap (learned the hard way)

**Never run two large models resident at once on a unified-memory Mac without doing the arithmetic
first.** A benchmark on this 48GB machine set a local Ollama model as an "LLM judge" alongside the
already-resident agent model: ~30GB + ~28GB = ~58GB against 48GB. Result: intermittent Ollama server
crashes (`HTTP 500`), which corrupted roughly a third of that run's trials — the failures were
infrastructure, not a measurement of anything real.

Before running two models together, check `ollama ps` (or `scripts/check.sh` in this skill) for
current resident VRAM, and compare against `cargo run --release -- machine`'s reported
`unified_memory_bytes`. If two models' `size_vram` would exceed available memory, don't co-resident
them — use a smaller second model, or run the second model as a genuinely separate process (a
different LLM-as-judge phase, not concurrent with the model under test — see the benchmark's own
`docs/05-grading-and-judge.md` for why the judge there is now a non-local model run in a separate
phase entirely, not a second local model).

**The root cause behind this incident**: `OLLAMA_MAX_LOADED_MODELS` is unset, so Ollama picks its
own cap — and that cap is **3**, not 1.

> **Correction.** An earlier version of this note said the default was `0`, meaning *unlimited*,
> citing [`envconfig/config.go`](https://github.com/ollama/ollama/blob/main/envconfig/config.go).
> That citation is real but the conclusion was wrong: the `0` there is a *sentinel*, and
> [`server/sched.go`](https://github.com/ollama/ollama/blob/main/server/sched.go) resolves it at
> load time to `defaultModelsPerGPU * gpu_count`, where `defaultModelsPerGPU = 3`. Ollama's own
> FAQ states the 3 directly. The research stopped one file short of where the sentinel is
> resolved — worth remembering as a failure mode: a real citation is not the same as a checked
> conclusion.

The conclusion survives the correction, because 3 is still far too many here: 3 x ~22GB does not
fit in 48GB, and Ollama's fit check is optimistic on unified memory, where "VRAM" and system RAM
are the same pool. A cap of 3 is what let the 58GB-on-48GB co-residency happen.

`doctor` (CLI and the MCP tool) surfaces this directly: an `ollama_env_config_warning` field fires
whenever the var is unset, with the exact fix (`launchctl setenv OLLAMA_MAX_LOADED_MODELS 1`, then
restart the Ollama app). Consistent with this skill's disk-cleanup policy of never automating a fix
that changes system state, `doctor` only reports it; applying the `launchctl setenv` is a human
decision.

### The other settings that move memory, and what they default to

`doctor` reports all nine with their **effective** defaults, because `launchctl getenv` returning
empty means "Ollama picks", which is not the same as "off" — reading a bare null as "unset =
unlimited" is precisely the mistake corrected above.

| Variable | Effective default | Why it matters here |
|---|---|---|
| `OLLAMA_MAX_LOADED_MODELS` | 3 x GPU count | See above |
| `OLLAMA_CONTEXT_LENGTH` | VRAM-tiered: 4k under 24GiB, 32k for 24-48GiB, **256k at 48GiB+** | The largest single memory lever. FreeLlama's routing always sends an explicit `num_ctx`, so anything going through `serve` is insulated — but a direct Ollama call on this 48GB machine inherits a 256k context |
| `OLLAMA_NUM_PARALLEL` | 1 | Memory scales by `NUM_PARALLEL x context_length`. Raising it multiplies KV-cache memory; it does not merely add scheduling slots |
| `OLLAMA_KV_CACHE_TYPE` | `f16` | `q8_0` roughly halves KV-cache memory at a given context length. Needs flash attention, which is already on — so this is usually available without setting anything |
| `OLLAMA_FLASH_ATTENTION` | **auto — on where the backend supports it, Metal included** | Not `off`. See the verification note at the end of this file |
| `OLLAMA_KEEP_ALIVE` | `5m` | How long a model holds memory after its last request |
| `OLLAMA_MAX_QUEUE` | 512 | Requests queued before Ollama starts rejecting |
| `OLLAMA_LOAD_TIMEOUT` | `5m` | A cold load of a large model can take minutes. Any client timeout below this gives up while Ollama is still working — the reason FreeLlama's task-path timeout is 900s while its control-plane timeout is 30s |
| `OLLAMA_GPU_OVERHEAD` | 0 | VRAM reserved away from model loading |

Beyond memory, check **placement**: `models` (`view: "resident"`) now derives a GPU/CPU split from `size_vram/size`
and warns when a model is partially offloaded to CPU. That is the quietest failure mode on this
setup — the model still answers, just many times slower, with no error anywhere.

## Picking a model for a task — and why zero-config `fastest` isn't actually "smart" yet

The `route` MCP tool (or `cargo run --release -- route --task <task> --objective
fastest|balanced|quality` without an agent) — needs `serve`, not bare `proxy` — see
`proxy-vs-serve.md`.

**Verified live on this machine:** `route --task code-repair --objective fastest` with no policy
configured returned `"selected_model": "qwen2.5:0.5b"`, `"confidence": "low"`,
`"evidence": "capability_metadata_only"` — a 0.5B model for code repair. This isn't a bug exactly:
`fastest` with zero benchmark evidence can only filter by capability metadata (does it advertise
`completion`+`tools`?) and pick by that alone, not by measured quality. If you don't first give it
real evidence, "fastest" can hand you a model too small to do the task well. This is the same trap
the generic "which model should I use" charts floating around the internet have — a name/param-count
table isn't evidence either, it's just a different kind of guess.

**The actual smart path (evidence-gated, not name-based):**
1. `cargo run --release -- bench-all` — runs `model_bench.rs`'s capability-aware benchmark across
   every installed model, sequentially, unloading between models. Produces real per-model measured
   evidence (`.octocode/evals/evidence/latest-all-models.json` by default), not spec-sheet guesses.
2. Turn that evidence into a policy: `platform.example.toml`'s `[policies.<task>]` sections list
   ordered `qualified_models` per task — this is what makes `balanced`/`quality` objectives
   meaningful (`recommend --task code-repair` with no policy returned
   `"installed_route_error": "no quality-qualified model exists for this task; configure a task
   policy..."` — it correctly refuses to vouch for anything rather than guess).
3. `cargo run --release -- route --task <task> --objective quality --policies platform.example.toml`
   now returns a genuinely evidence-backed pick for *your* installed models on *your* machine
   (`cargo run --release -- machine` reports the real hardware it reasons about — chip, unified
   memory, CPU count — not a generic hardware tier).

`cargo run --release -- recommend --task <task>` separately proposes an *install* plan (side-effect-
free — never runs `ollama pull` itself) from `recommendations.example.toml`'s catalog, for models you
don't have yet.

**Bottom line:** the ingredients for "smart, hardware-matched model selection" all exist and are
better than a static chart once fed evidence (`bench-all` → policy) — but out of the box, with zero
setup, `fastest` is a capability filter, not a quality judgment. Don't trust a zero-config `fastest`
pick for anything quality-sensitive; run `bench-all` first.

## Recommended Ollama configuration for this machine (52GB M4 Pro, 14 cores)

Every `OLLAMA_*` variable is currently **unset**, so Ollama picks its own defaults — and two of
those defaults are actively wrong for this hardware and model set. `doctor` reports the live values
with their effective defaults; this is the interpretation.

| Setting | Now | Recommended | Why |
|---|---|---|---|
| `OLLAMA_MAX_LOADED_MODELS` | unset → **3** | **1** | Two ~20GB models are installed. Three co-resident is ~60GB against 52GB of unified memory. This exact condition already crashed this machine |
| `OLLAMA_FLASH_ATTENTION` | unset → **auto (on, on Metal)** | leave unset | Already on where supported; setting `1` changes nothing here, `0` would disable the row below |
| `OLLAMA_KV_CACHE_TYPE` | unset → `f16` | **`q8_0`** | Roughly halves KV-cache memory at a given context length, and its prerequisite is already satisfied. Matters most because of the row below |
| `OLLAMA_CONTEXT_LENGTH` | unset → **256K** | consider an explicit value | The VRAM-tiered default reaches 256K at 48GB+. FreeLlama's routing always sends an explicit `num_ctx`, so anything through `serve` is insulated — but a direct `/api/chat` call inherits 256K and the KV cache that implies |
| `OLLAMA_KEEP_ALIVE` | unset → `5m` | fine | Per-request `keep_alive` already overrides it; use `"0"` for one-offs |
| `OLLAMA_LOAD_TIMEOUT` | unset → `5m` | fine | A cold 20GB load can genuinely take minutes; this is why FreeLlama's task-path timeout is 900s |

Apply with `launchctl setenv <VAR> <value>` and **restart the Ollama app** — `launchctl` sets the
launchd session environment, which is what the app inherits at launch, not what a running process
sees. Consistent with this skill's disk-cleanup policy, `doctor` only reports these; applying them
is a human decision.

Caveat on reading them back: `launchctl getenv` cannot see variables set for a server started from
a shell (`OLLAMA_CONTEXT_LENGTH=64000 ollama serve`). A `null` from `doctor` means "not set via
launchd", not "definitely unset".

## Quality of the models currently installed

Measured on this machine, not inferred from model cards. Full method in
`references/task-delegation.md` and `.octocode/evals/2026-08-30-small-model-deep-research-eval.md`.

| Model | Size | Code research | Vision | Verdict |
|---|---|---|---|---|
| `qwen3.8:27b-mlx` | 18GB | **8/8** grounded lookups @ 11.8s median · 7/10 real-repo · 86.7% on the 30-question suite | UI/chart ✓ ~37s, transcription ✓ ~17s | **Default for everything.** Best accuracy *and* fastest of the two |
| `muse-glimmer:30b-mlx` *(removed)* | 21GB | 7/8 @ 17.9s · 6/10 real-repo · **96.7%** on the 30-question suite, zero failed tool calls | UI ✓ ~45s, transcription ✓ ~39s | Keep as the specialist: it wins the largest-sample benchmark and never fumbles a tool call |
| `nomic-embed-text` | 0.3GB | — | — | 322 chunks (159k local tokens) in 9.6s. The most-pulled embedding model on ollama.com by a wide margin (83.9M) |

**Update:** `muse-glimmer` was removed; `qwen3.8:27b-mlx` now covers vision, coding, OCR and
summarisation alone, and got *faster* without the memory contention (vision 37s -> 13.7s, OCR
17s -> 9.7s). The comparison below is retained as the evidence behind that decision.

The two large models were **not redundant** — qwen wins two of three code benchmarks, muse wins the
third by a distance. Neither dominates. If memory ever forces a choice, drop muse and accept worse
narrow-lookup accuracy.

### What else is worth considering

Surveyed with `search_models` (popular ordering). The three most popular models overall
(`glm-5.3`, `glm-5.3-flash`, `deepseek-v4-flash`) are all **cloud-only** and cannot run locally —
irrelevant for offload no matter how they rank. Among local candidates not installed:
`nemotron-3.5-lightning` (tools + thinking) and `qwen3.8-flash-next` (an experimental preview of
the next qwen architecture) are the notable ones. Neither has measured evidence here, so by this
skill's own rule they would return a `verify` verdict from `delegate_research` until benchmarked.

## Making `minConfidence: "medium"` actually reachable

This is the step nobody does, which is why the fail-closed gate degrades to refusing everything.
`route_evidence` grades a route `medium` only when the task has **both** inputs:

| Input | Supplies | Without it |
|---|---|---|
| `--policy-file` | a *quality* contract: which models are vouched for on this task | `low`, and `objective: balanced/quality` errors outright |
| `--benchmark-report` | local *functional* measurement from `freellama bench-all` | `low`, evidence `configured_task_policy` |

Neither alone is enough, and that is deliberate: a policy without measurement is an unverified
claim, and measurement without a policy is throughput with nobody vouching for correctness.

Generate the policy from **quality** data, never from `bench-all`:

```bash
freellama policy-from-eval \
  --aggregate benchmark/local/results/<model>/aggregate.json \
  --task coding --min-pass 0.8 --out platform.toml

freellama serve --policy-file platform.toml --benchmark-report <bench-all output>.json
```

`bench-all` measures `decode_tokens_per_second`. Generating a policy from it would relabel speed as
a quality contract and make `medium` reachable with no new correctness evidence — worse than the
gate refusing everything, because it would pass while meaning nothing. `policy-from-eval` therefore
reads harness aggregates, which carry `pass_at_1`.

It refuses to manufacture evidence it does not have:

- **fewer than 3 trials** → refuses, because the harness's own rule is that one trial is a smoke
  result. `--allow-smoke` writes the policy with a SMOKE-ONLY banner in the file.
- **past `review_due_at`** → refuses.
- **nothing clears the threshold, or the model is not installed here** → refuses.

Provenance (source aggregate, benchmark date, threshold, trial count per model) is written into the
generated file, so a stale contract is visible without archaeology.

## Verified against upstream on 2026-08-31 — two advisories were wrong

Re-checked all nine `doctor` advisories against `docs/faq.mdx`, `docs/context-length.mdx` and
`envconfig/config.go` at ollama/ollama main, plus the installed build (server 0.33.2).
Seven matched exactly. Three things are worth carrying forward.

**`OLLAMA_FLASH_ATTENTION` was documented as `off`; it is auto-enabled on supported backends,
Metal included.** The tables above now say so directly. The lesson worth keeping is *why* it was
wrong: the variable is declared `BoolWithDefault("OLLAMA_FLASH_ATTENTION")`, whose whole purpose is
to let the caller supply the default (plain `Bool` is the one pinned to `false`), and the `false`
visible in envconfig's describe-map is a help-listing display value, not the runtime resolution.
**Same error as the `MAX_LOADED_MODELS` sentinel: a declaration is not a resolved value.** Pinned by
`flash_attention_is_not_advertised_as_off_by_default` in `tests/suite_contract.rs`.

**The CLI's own `--help` can be the stale source.** The installed CLI (0.13.5) prints
`OLLAMA_CONTEXT_LENGTH ... (default: 4096)`, while the running server (0.33.2) and
`docs/context-length.mdx` both use the VRAM-tiered default (4k / 32k / 256k). `ollama serve
--help` is only authoritative when the CLI and server are the *same* build — `doctor` already
reports that mismatch, so read the two together.

**`num_ctx=8192` is below upstream's floor for this workload.** `docs/context-length.mdx` says
plainly: "Tasks which require large context like web search, **agents**, and coding tools should be
set to at least 64000 tokens." Both research adapters run at 8192, 8x under that, which is exactly
why long runs silently overflowed the window (see `AGENTS.md`, Context management). Raising it
multiplies KV-cache memory, so it is a real trade-off on a 48GB box — but the mitigation upstream
recommends is available and unused here: `OLLAMA_KV_CACHE_TYPE=q8_0` is "approximately 1/2 the
memory of f16 with a very small loss in precision ... recommended", and it needs flash attention,
which per the correction above is already on. Measured on this machine during a sustained
delegation run, resident memory grew from 19.5GB to 27GB on KV cache alone at `num_ctx=8192`.
