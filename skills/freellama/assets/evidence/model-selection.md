# Picking a model, and making the confidence gate mean something

Load when choosing a model for a task, finding one you do not have yet, or configuring routing so
`minConfidence:"medium"` is reachable. Memory arithmetic and `OLLAMA_*` tuning live in
`references/ollama-config.md`; this file owns *which model*.

## Zero-config `fastest` is a capability filter, not a quality judgment

**Verified live:** `npx @octocodeai/freellama route --task code-repair --objective fastest` with no policy
configured returned `"selected_model": "qwen2.5:0.5b"`, `"confidence": "low"`,
`"evidence": "capability_metadata_only"` — a 0.5B model for code repair. Not a bug: with zero
evidence, `fastest` can only filter on advertised capability (`completion` + `tools`) and pick by
that. A name/param-count table is not evidence either, just a different guess.

So: pass `minConfidence:"medium"` for anything quality-sensitive, and expect a refusal until the
two inputs below exist. `objective: "balanced"` and `"quality"` need a configured policy at all;
`"fastest"` does not.

## Making `minConfidence:"medium"` reachable — the step nobody does

`route_evidence` grades a route `medium` only when the task has **both**:

| Input | Supplies | Without it |
|---|---|---|
| `--policy-file` | a *quality* contract: which models are vouched for on this task | `low`, and `objective: balanced/quality` errors outright |
| `--benchmark-report` | local *functional* measurement from `npx @octocodeai/freellama bench-all` | `low`, evidence `configured_task_policy` |

Neither alone is enough, deliberately: a policy without measurement is an unverified claim, and
measurement without a policy is throughput with nobody vouching for correctness.

Generate the policy from **quality** data, never from `bench-all`:

```bash
npx @octocodeai/freellama policy-from-eval \
  --aggregate <harness aggregate.json> --task coding --min-pass 0.8 --out platform.toml
npx @octocodeai/freellama serve --policy-file platform.toml --benchmark-report <bench-all output>.json
```

`bench-all` measures `decode_tokens_per_second`. Generating a policy from it would relabel speed as
a correctness contract and make `medium` reachable with no new quality evidence — worse than the
gate refusing everything, because it would pass while meaning nothing. `policy-from-eval` reads
harness aggregates instead, which carry `pass_at_1`, and refuses to manufacture evidence it lacks:
fewer than 3 trials (one trial is a smoke result; `--allow-smoke` writes it with a banner), past
`review_due_at`, nothing clearing the threshold, or a model not installed here. Provenance — source
aggregate, date, threshold, trials per model — is written into the generated file, so a stale
contract is visible without archaeology.

## Finding a model you do not have — `models {view:"library"}`, two steps

1. **Search** (`query`, `capabilities`, `order`) returns *family* names, popular-ordered. A family
   is **not pullable**. `cloudOnly` marks models that only run on Ollama's hosted service. Site
   rank is not pull count — judge with `pulls`, not position.
2. **Inspect** (`model:"<family>"`) returns each tag with size, context window, modalities, and
   `fitsInMemory` computed against this machine.

Pulling from step 1 alone means guessing the size, which is how a 143GB tag looked like a candidate
on a 48GB machine. Step 2 **fails closed**: with `serve` unreachable there is no machine profile, so
no tag is recommended and you get `recommendationUnavailable`. An earlier fail-open version, asked
with serve down, recommended the 143GB `qwen3-vl:235b`. Start `serve` and ask again; never fall back
to picking the biggest tag yourself.

`npx @octocodeai/freellama recommend --task <task>` separately proposes an *install plan* from a reviewed
catalog — side-effect-free, it never runs `ollama pull`. Not on the MCP surface.

## Quality of the models installed here — measured, not inferred

Full method: `references/task-delegation.md` and `references/model-profile-qwen3.8-27b-mlx.md`.

| Model | Size | Code research | Vision | Verdict |
|---|---|---|---|---|
| `qwen3.8:27b-mlx` | 18GB | **8/8** grounded lookups @ 11.8s median · 7/10 real-repo · 86.7% on the 30-question suite | UI/chart ✓, transcription ✓ | Preferred measured generalist for bounded local work; independently verify judgment |
| `muse-glimmer:30b-mlx` *(removed)* | 21GB | 7/8 @ 17.9s · 6/10 real-repo · **96.7%** on the 30-question suite, zero failed tool calls | UI ✓ ~45s, transcription ✓ ~39s | Won the largest-sample benchmark; removed for memory |
| `nomic-embed-text` | 0.3GB | — | — | 322 chunks (159k local tokens) in 9.6s. Most-pulled embedding model on ollama.com by a wide margin |

The two large models were **not redundant** — qwen won two of three code benchmarks, muse won the
third by a distance. Neither dominated. `muse-glimmer` was dropped anyway, and qwen got *faster*
without the memory contention (vision 37s → 13.7s, OCR 17s → 9.7s), which is the more useful lesson:
**freeing contention beat adding a specialist.**

Embedding model choice matters and popularity does not predict it: `qwen3-embedding` ranks first on
ollama.com and scored **4/6 recall@3 at 3.5× the indexing cost** of `nomic-embed-text` (5/6, 274MB).
`embeddinggemma:300m` tied on quality at twice the size.

Among local candidates not installed, `nemotron-3.5-lightning` (tools + thinking) and
`qwen3.8-flash-next` are the notable ones. Neither has measured evidence here, so by this skill's
own rule they would return a `verify` verdict until benchmarked. The three most popular models
overall (`glm-5.3`, `glm-5.3-flash`, `deepseek-v4-flash`) are **cloud-only** — irrelevant for
offload no matter how they rank.

## Vision: send a real image, because tags lie in both directions

**Verified 2026-08-30, not inferred.** `qwen3.8:27b-mlx` described a UI and a bar chart accurately
(~37s) and transcribed text accurately (~17s), spelling `freellama` correctly.

An earlier version of this file said both large models were "untested, don't assume"; a later audit
concluded there was *no working vision model installed at all*. Both were wrong the same way: only
models whose **names** suggested vision were tested, and the two large models already in daily use
for text were never tried. **The thing to verify is the behaviour, not the tag.**

Three models were removed after testing, and the reasons are worth keeping:

- `llama3.2-vision:latest` — **could not load at all**: `unknown model architecture: 'mllama'`, in
  Ollama's own server log. The GGUF declares an architecture the running `llama-server` does not
  recognise; consistent with the CLI/server version drift `doctor` flags. Remedy is
  `brew upgrade ollama` (align CLI with the running server), then re-pull.
- `deepseek-ocr:latest` — OCR'd fast (~7s) but **dropped a character** (`freellama` → `freelama`)
  and **degenerated into a repeating loop** on an image with no text, leaking chat-template tokens.
- `gemma4:12b-mlx` — **rejected image input outright** despite the family being multimodal; the MLX
  conversion dropped the vision head.

`glm-ocr:latest` is now installed and passed the checked repository screenshot: it returned all
four visible text segments exactly. With unconstrained output it appended repeated Markdown fences;
`options.stop:["```"]` produced the exact plain-text transcription. This is a model-specific
decoding contract, not a generic vision default.

**Verified again 2026-09-01 through authenticated managed execution.** A held-out one-line image
read `FREELLAMA OCR 2026`. Fence-only stopping recognized the text but repeated it to the output
cap, so the production gate rejected the run. Adding a newline stop for that explicitly one-line
contract returned the exact text and a verified GPU placement receipt. Do not apply the newline
stop to multiline OCR; define a task-specific terminator or postcondition instead.

**`doctor` catches a version mismatch, but only invoking a model catches an architecture gap.**

Getting an image through FreeLlama's own routing works end to end: `run_task`'s `images` parameter
(base64, no data-URI prefix) attaches to the `prompt` message and is forwarded verbatim — verified
through the full MCP → `run_task` → `/_freellama/v1/tasks` → Ollama chain. Before that parameter
existed, `route`'s `requiredCapabilities:["vision"]` picked a capable model correctly but there was
no way to hand it an image.

Next: will the pick fit in memory, and how is it tuned? → `references/ollama-config.md`. Deciding
what to hand it at all → `references/task-delegation.md`.
