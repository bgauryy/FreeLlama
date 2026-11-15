# Benchmark commands

The benchmark definition, results and limits live in [BENCHMARK.md](../../docs/BENCHMARK.md).
The example has seven entries run one at a time: Ollama alone, FreeLlama managed tasks, FreeLlama
streamed managed tasks, llama.cpp, llama-swap, LocalAI and oMLX. `freellama-stream` is disabled in
the example; managed streaming is implemented, so enable that entry when including it in the
comparison. oMLX is enabled with a placeholder model ID: supply a separately qualified matching
MLX artifact or disable it explicitly before running. The FreeLlama benchmark entries target
Ollama-backed managed tasks; the [explicit managed llama.cpp adapter](../../docs/LLAMACPP.md)
is a separate integration path.

```sh
# Optional native-Mac software setup; selected competitors only, no models/daemons:
bash scripts/bench/setup-macos.sh --install
# llama-swap: download the GitHub release archive and check its SHA-256; see llama-swap.example.yaml.
# LocalAI: install the llama-cpp-development backend; see localai-model.example.yaml.
# Read-only file, model-identity, endpoint and capacity checks:
node scripts/bench/preflight.mjs --config scripts/bench/vendors.example.json --hash-model
# Set model.ollama_tag and its artifact_sha256 once; Ollama and FreeLlama inherit the tag, and the
# other vendors load the same GGUF file. Start the required services first:
node scripts/bench/run.mjs --config scripts/bench/vendors.example.json --samples 30 --out NEW_DIRECTORY
```

Output: `report.html`, `summary.json`, `summary.csv`, `SUMMARY.md`, plus full per-vendor trial and
host observations. HTML does not rank unqualified or different-artifact cohorts. `--vendor ID`
runs one configured block; `--allow-smoke` permits fewer than 30 samples and cannot qualify a win.

Native CPU sanity (separate from the task comparison):

```sh
node scripts/bench/sanity.mjs --model /path/to/verified.gguf --digest EXACT_SHA256 --out NEW_SANITY_DIRECTORY
node scripts/bench/report.mjs --report RUN_DIRECTORY/summary.json --sanity NEW_SANITY_DIRECTORY/summary.json --out NEW_REPORT.html
```

`sanity.mjs` verifies model bytes, runs offline CPU work only, and records process wall/user/system
time using `/usr/bin/time -p`. Its CPU core-equivalent is `(user+system)/wall`; process RSS is not
collected. This includes startup, loading and warm-up. It is not the warm-task latency KPI.

The common CPU KPI uses whole-host OS scheduler deltas, averaged over sampled intervals including
warm-ups; load average is not CPU utilization. Report coverage and keep the background workload
fixed. Memory means minimum available bytes per block. On Linux that is `MemAvailable`; on macOS it
is labelled `freellama_estimate`: free + max(min(inactive + speculative, file-backed), purgeable),
capped at total RAM, the same bounded formula as the core. It is not a kernel value. Each host
sample also records RSS for the owned process group and, for Ollama-backed protocols, Ollama runner
processes; summaries report the peak. Ollama `/api/ps` `size_vram` is recorded at block start and
end. RSS and `size_vram` are never added: on unified memory RSS undercounts GPU buffers. Local model
tokens and paid-agent tokens are distinct; the latter has no meter yet.

## Run options

| Flag | Effect |
|---|---|
| `--keep-loaded` | Skip the unload after Ollama-backed blocks (`ollama`, `freellama`, `freellama-managed`, `freellama-stream`). Without it, the runner sends `keep_alive: 0` for the bench model to `ollama_base_url` (config, default `http://127.0.0.1:11434`) and confirms with `/api/ps`. It unloads only a model that was not resident before the block (or that this run loaded earlier), and never after an uncertain inference. The outcome is in `unload`. |
| `--max-load-per-cpu X` | At each block start, record 1-minute load per CPU and the top 5 CPU processes (`load-gate.json`); warn above X. |
| `--strict-load` | With `--max-load-per-cpu`, skip a block above X. It is recorded as `not_run`, not failed. |
| `--concurrency 2,4,8` | After the sequential samples, send the sample count through N concurrent clients per level on the warm model. Records p50/p95, failures and wall-time throughput (`concurrency-N.jsonl`, `concurrency.csv`). Without it, output is unchanged. |
| `--cold [--cold-samples 3]` | Once per block, after the warm samples: for each cold sample, unload (Ollama-backed: `keep_alive: 0`, confirmed by `/api/ps`) or restart the owned process, then time one request. Records first-request time, vendor-reported load time and restart-to-ready time (`cold.jsonl`). Vendors with neither are `unsupported`. Cold mode unloads the bench model even if it was resident before. |
| `--hog-gb N [--force-hog]` | With `--cold`: start `memory-hog.mjs` (allocates and touches N GiB) before each block and stop it after; records swap before, after ready and after stop. Refused, and the block recorded `not_run`, when N exceeds estimated available minus 2 GiB or available is unknown, unless `--force-hog`. |

`node scripts/bench/memory-hog.mjs --gb N [--force]` can also run alone; it holds memory until
SIGTERM or SIGINT.

Preflight treats a vendor with `start` as `owned_start`: it is not running yet, so its endpoint is
not probed; only the start command is checked (found on `PATH` or under `start.cwd`, executable).
Ollama model identity is the manifest's `application/vnd.ollama.image.model` layer digest checked
against the hashed blob; the `/api/tags` tag digest is printed as information only.

`report.mjs --report A/summary.json --report B/summary.json --out NEW.html` renders a pooled table
(p50/p95 from every verified measured trial in each run's `VENDOR/trials.jsonl`) and one table per
run order. A single `--report` renders as before.

Choose a GGUF chat model supported by every selected engine and its chat template. The example
names the one the dated run used; pick one that fits your machine, then run preflight with
`--hash-model` to check every vendor loads the same bytes.

Cohort is a declaration, not verification. All entries in the GGUF cohort use the same artifact;
oMLX needs a separately qualified matching MLX artifact. A declared MLX cohort never inherits the GGUF hash.
Replace `SET_MATCHING_MLX_MODEL_ID` and check all effective templates, contexts and cache policies.
No model download is automatic; new pulls require the exact artifact/tag and reported size approved.

Unknown inference stops every later sample/vendor. Stopping an owned frontend does not prove its
external engine stopped. The checkout lock coordinates this harness only; later manual runs require
independent confirmation of engine quiescence. Optional `start:{command,args,cwd,env,ready_timeout_ms}`
uses argv, never a shell; only owned POSIX groups are stopped. Windows services must start externally.
Responses are capped at 8 MiB, redirects are rejected, environment values are redacted, and secrets
must not be placed in argv. Existing output folders are never overwritten. Failed warm-ups and
all failed measured effort remain recorded; token totals require complete usage coverage.

Run a second pass with the enabled vendors in reverse order before comparing: on a shared host,
drift between passes can exceed the gaps between systems.

`yarn test:bench` checks deterministic fixtures with no model downloads or live inference.
Per-machine model calibration uses [bench-all](../../docs/MODEL_SELECTION.md#turn-measurements-into-routing-evidence);
model tool-calling and check-line evaluation use the separate [reliability harnesses](../../docs/RELIABILITY.md).
Vendor latency, local throughput and model correctness answer different questions.
