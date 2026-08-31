# Ollama configuration and the memory arithmetic

Load before co-residenting models, changing an `OLLAMA_*` variable, or explaining why a model
suddenly answers many times slower with no error. This file owns *whether a model fits and how it
is tuned*; `model-selection.md` owns *which* model.

## Check placement first — the quietest failure on this setup

`models {view:"resident"}` derives a GPU/CPU split from `size_vram/size` and emits
`placement.warning` when a model is partially offloaded to CPU. It still answers, just many times
slower, with no error anywhere. Free VRAM (`ollama_manage {action:"stop"}`) or lower the context.

## Never co-resident two large models without the arithmetic

A benchmark on this 48GB machine ran a local "LLM judge" alongside the already-resident agent
model: ~30GB + ~28GB = ~58GB against 48GB. Result — intermittent Ollama `HTTP 500`s that corrupted
roughly a third of the run's trials. The failures measured nothing real; they were infrastructure.

Before running two together, compare resident `size_vram` (`models {view:"resident"}`, or
`ollama ps`) against `unified_memory_bytes` from `doctor`'s machine profile. Budget ~60% of memory
for one model — the KV cache and anything already resident must fit too. A small model (vision
~7.8GB, embedding ~300MB) beside one large model (~18-30GB) is comfortably safe; two large ones are
the combination that crashed a run here.

**`ollama stop <model>` / `ollama_manage {action:"stop"}`** unloads immediately instead of waiting
out `keep_alive` — do that before loading a second large model rather than trusting the idle timer.

**Root cause of that incident: `OLLAMA_MAX_LOADED_MODELS` is unset, and unset does not mean 1.**
It is declared `0` in [`envconfig/config.go`](https://github.com/ollama/ollama/blob/main/envconfig/config.go),
which reads as "unlimited" — but [`server/sched.go`](https://github.com/ollama/ollama/blob/main/server/sched.go)
resolves that sentinel at load time to `defaultModelsPerGPU * gpu_count`, i.e. **3** on a
single-GPU machine, which Ollama's own FAQ states directly. An earlier version of this file stopped
one file short and shipped the wrong advisory for weeks. **A declaration is not a resolved value** —
read `envconfig` and `sched.go` together, always. The conclusion survives either way: 3 × ~22GB does
not fit in 48GB, and Ollama's fit check is optimistic on unified memory, where "VRAM" and system RAM
are one pool.

`doctor` fires `ollama_env_config_warning` whenever the var is unset, naming the fix
(`launchctl setenv OLLAMA_MAX_LOADED_MODELS 1`, then restart the Ollama app).

## The nine settings `doctor` reports, with effective defaults

`launchctl getenv` returning empty means "Ollama picks", which is not "off" — reading a bare null as
"unset = unlimited" is exactly the mistake corrected above.

| Variable | Effective default | Recommended here | Why it matters |
|---|---|---|---|
| `OLLAMA_MAX_LOADED_MODELS` | 3 × GPU count | **1** | See above. Two ~20GB models are installed; three co-resident is ~60GB against 52GB |
| `OLLAMA_CONTEXT_LENGTH` | VRAM-tiered: 4k under 24GiB, 32k for 24-48GiB, **256k at 48GiB+** | consider an explicit value | The largest single memory lever. FreeLlama always sends an explicit `num_ctx`, so anything through `serve` is insulated — a direct `/api/chat` call inherits 256k and the KV cache that implies |
| `OLLAMA_KV_CACHE_TYPE` | `f16` | **`q8_0`** | Roughly halves KV-cache memory at a given context length, "with a very small loss in precision … recommended" upstream. Needs flash attention, already on |
| `OLLAMA_FLASH_ATTENTION` | **auto — on where the backend supports it, Metal included** | leave unset | Not `off`. Setting `1` changes nothing here; `0` would disable the row above |
| `OLLAMA_NUM_PARALLEL` | 1 | raise only with `q8_0` | Memory scales by `NUM_PARALLEL × context_length` — it multiplies KV-cache memory, it does not merely add scheduling slots. At 1, FreeLlama's shared admission permits cannot buy parallel decoding: two concurrent requests against a resident model measured 1.12× here |
| `OLLAMA_KEEP_ALIVE` | `5m` | fine | Per-request `keep_alive` overrides it; use `"0"` for one-offs |
| `OLLAMA_LOAD_TIMEOUT` | `5m` | fine | A cold 20GB load genuinely takes minutes — which is why FreeLlama's task-path timeout is 900s and its control-plane timeout 30s |
| `OLLAMA_MAX_QUEUE` | 512 | fine | Requests queued before Ollama itself starts rejecting |
| `OLLAMA_GPU_OVERHEAD` | 0 | fine | VRAM reserved away from model loading |

Apply with `launchctl setenv <VAR> <value>` and **restart the Ollama app** — `launchctl` sets the
launchd session environment the app inherits at launch, not what a running process sees. Consistent
with this skill's disk-cleanup policy, `doctor` only reports; applying is a human decision.

Reading them back: `launchctl getenv` cannot see variables set for a server started from a shell
(`OLLAMA_CONTEXT_LENGTH=64000 ollama serve`). A `null` from `doctor` means "not set via launchd",
not "definitely unset".

## Where FreeLlama's own tuning lives

- [`model_bench.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/model_bench.rs)'s `benchmark_plan()` — tiers `num_ctx` by model size (8192
  for models ≥16GB, 16384 for ≥2GB, 32768 below that), capped by the model's advertised context.
  `BenchmarkConfiguration` defaults: `num_predict=128`, `temperature=0`, `seed=42`, `think=false`,
  `keep_alive="5m"`.
- [`routing.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/platform/routing.rs)'s `profile()` — per-task tuning (Browser→64 tokens,
  Tools→256, CodeRepair→2048, Embedding→0), plus a `qwen_repair_profile` special case pinning
  `num_ctx=8192` and `num_predict=512` for `qwen3.8:27b-mlx` on code-repair only.
- [`platform/mod.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/src/platform/mod.rs)'s `managed_execution: Arc<RwLock<()>>` — resident-model
  tasks take a shared read lock and run concurrently; a model-swap task takes the exclusive write
  lock, so transitions cannot race each other.

## Two upstream advisories that were wrong, and why

Re-checked against Ollama's own [`faq.mdx`](https://github.com/ollama/ollama/blob/main/docs/faq.mdx), [`context-length.mdx`](https://github.com/ollama/ollama/blob/main/docs/context-length.mdx)
and [`envconfig/config.go`](https://github.com/ollama/ollama/blob/main/envconfig/config.go), plus the installed build (server 0.33.2). Seven of nine matched exactly.

**`OLLAMA_FLASH_ATTENTION` was documented here as `off`; it is auto-enabled on supported backends,
Metal included.** The variable is declared `BoolWithDefault("OLLAMA_FLASH_ATTENTION")`, whose whole
purpose is to let the caller supply the default (plain `Bool` is the one pinned to `false`), and the
`false` in envconfig's describe-map is a help-listing display value, not the runtime resolution.
Same error as the `MAX_LOADED_MODELS` sentinel. Pinned by
`flash_attention_is_not_advertised_as_off_by_default` in [`suite_contract.rs`](https://github.com/bgauryy/FreeLlama/blob/main/packages/rust-core/tests/suite_contract.rs).

**The CLI's own `--help` can be the stale source.** The installed CLI (0.13.5) prints
`OLLAMA_CONTEXT_LENGTH … (default: 4096)`, while the running server (0.33.2) and
Ollama's `context-length.mdx` both use the VRAM-tiered default. `ollama serve --help` is authoritative
only when CLI and server are the *same* build — `doctor` reports that mismatch, so read both.

## One known gap: `num_ctx=8192` is below upstream's floor for agents

Ollama's [`context-length.mdx`](https://github.com/ollama/ollama/blob/main/docs/context-length.mdx) says plainly that tasks requiring large context — "web search,
**agents**, and coding tools" — should be set to at least 64000 tokens. Both research adapters run
at 8192, 8× under that, which is why long runs silently overflowed the window (see [`AGENTS.md`](https://github.com/bgauryy/FreeLlama/blob/main/AGENTS.md),
Context management). Raising it multiplies KV-cache memory: measured during a sustained delegation
run here, resident memory grew from 19.5GB to 27GB on KV cache alone *at* 8192. The mitigation
upstream recommends is `OLLAMA_KV_CACHE_TYPE=q8_0`, available and unused.

Next: choosing what to run in that memory → `references/model-selection.md`. Seeing a symptom
rather than planning a change → `references/troubleshooting.md`.
