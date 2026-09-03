# Troubleshooting: symptom → cause → fix

Load when something is failing. Every entry was observed and diagnosed live, not theorised.
Run `doctor` first regardless — it is the only tool that works with `serve` down.

## `503 server busy` from a managed task

**Not a failure.** Admission uses independent cost-unit budgets (embedding 1, chat 2, vision 4;
GPU 2 and CPU 1 by default). A task that cannot get a backend permit within 120s is refused, naming
the cost and budget. **Fix:** retry, or lower your fan-out. `GET /_freellama/v1/health` reports each
backend's `slots_available` before you submit, and
every success reports `admission.queue_wait_ms` so throttling is visible before it becomes a 503.
Raise the GPU ceiling with `--max-concurrent-tasks` or CPU with
`--cpu-max-concurrent-tasks`, but read
`references/ollama-config.md` first — `OLLAMA_NUM_PARALLEL` defaults to 1, so extra slots bound the
burst rather than buying parallel decoding.

## A route is refused with "confidence is low"

**Not a failure either** — a fail-closed refusal. With neither a policy file nor a benchmark report
every route grades `low`, so `minConfidence:"medium"` refuses everything. **Fix:** lower the floor
for this call, name an explicit `model` (which does *not* raise the grade — the grade measures the
evidence, not who chose), or configure the two inputs. → `references/model-selection.md`

## A model answers, but many times slower than usual

**Cause:** it spilled to CPU. **Fix:** `models {view:"resident"}` — a `placement.warning` with a
`gpu_percent` below 100 confirms it. Free VRAM (`ollama_manage {action:"stop"}`) or lower the
context length. No error is raised anywhere for this; it is the quietest failure mode here.

## `HTTP 500` from `/api/chat`, intermittent

**Cause A — two large models resident at once.** Check `models {view:"resident"}` (or `ollama ps`);
more than one large model listed is almost certainly it. → `references/ollama-config.md`

**Cause B — Ollama's own transient flakiness under sustained tool-calling load**, independent of
memory: measured ~8% per-request even with one resident model and headroom to spare. **Fix:** send
requests through `npx @octocodeai/freellama proxy`/`serve`, not raw Ollama — the proxy retries these
automatically. `scripts/check.sh` reports whether the request uses it.

## A request that used to fail fast now times out instead

**Cause:** retries without a bounded per-attempt timeout compound. One slow failing
request (15-18s measured) retried 2-3 times blows past the caller's own budget, turning a fast clean
failure into a slow total loss. **Fix:** the proxy sets `--request-timeout-seconds` (default 120) for
exactly this reason; give any retry wrapper of your own the same discipline, and raise the *caller's*
budget to leave room (this repo raised task timeouts from 180s to 240s). → `references/reliability.md`

## `models` / `route` returns 404

**Cause:** you are pointed at `freellama proxy` (passthrough only), not `freellama serve` (full
platform) — different route sets on the same default port. → `references/proxy-vs-serve.md`

## `delegate_research` fails with "outside the allowed research roots"

**Cause:** `workspacePath` resolved (through symlinks) outside `FREELLAMA_MCP_ALLOWED_ROOTS`, which
defaults to the FreeLlama checkout. The error names the resolved path and the roots. **Fix:** point
it at a directory inside a root, or set `FREELLAMA_MCP_ALLOWED_ROOTS` (colon-separated) if you
genuinely need another tree. Do not widen it to `$HOME` or `/`: an unconstrained version of this tool
was verified listing a real home directory.

## `ollama` CLI and server report different versions

**Cause:** the Ollama app auto-updated but the running server process is the old binary (or the
reverse, for a Homebrew CLI). `doctor` surfaces this explicitly, and it matters because
`ollama serve --help` is only authoritative when both are the same build. **Fix:** fully quit and
relaunch the Ollama app (`osascript -e 'quit app "Ollama"'`, then `open -a Ollama`).

## A background run outlived its lifetime and raced a later one

**Cause:** a long-running job was not stopped before starting another pointed at the same output
directory; both wrote into it concurrently and the "latest complete run" logic got confused by
interleaved partials. **Fix:** check for stragglers before launching anything against a shared
results directory, kill what you did not expect, clear the directory, and only then start. Never
assume a background task you did not watch finish is gone.

Next: placement/admission decisions → `references/resource-routing.md`; changing retry/timeout
behaviour rather than diagnosing it → `references/reliability.md`.
