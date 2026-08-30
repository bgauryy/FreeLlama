# Troubleshooting: symptom → cause → fix

Load when something is actually failing and you need symptom-to-cause-to-fix, not a config choice.

All of these were observed and diagnosed live on this machine, not theoretical.

## `HTTP 500` from `/api/chat`, intermittent

**Cause A — two large models resident at once.** Check `ollama ps`; if more than one model is
listed, or `scripts/check.sh` warns about total resident VRAM, this is almost certainly it. See
`model-selection.md`. Fix: don't co-resident large models; check memory arithmetic first.

**Cause B — Ollama's own transient flakiness under sustained tool-calling load**, independent of
memory (measured ~8% per-request rate even with one resident model and headroom to spare). Fix: run
requests through `freellama proxy`/`serve`, not raw Ollama — `packages/rust-core/src/proxy.rs` retries these
automatically (see `reliability.md`). Verify you're actually going through the proxy:
`scripts/check.sh` reports this.

## A request that used to fail fast now times out instead

**Cause:** retries without a bounded per-attempt timeout compound. A single slow-but-eventually-
failing request (measured 15-18s each in one case) retried 2-3 times can blow past a caller's own
timeout budget, turning a fast, clean failure into a slow, total-data-loss timeout. Fix: this
repo's proxy sets `--request-timeout-seconds` (default 120s) for exactly this reason — if you built
your own retry wrapper elsewhere, give it the same discipline. Also worth raising the *caller's* own
timeout/budget to give the retry+timeout combination room to work (this benchmark raised task
timeouts from 180s to 240s for this reason).

## `freellama models`/`route` returns 404

**Cause:** you're pointed at `freellama proxy` (passthrough-only), not `freellama serve` (full
platform). These are different route sets on the same default port. See `proxy-vs-serve.md`.
`scripts/check.sh` tells you which one is running.

## `ollama` CLI and server report different versions

**Cause:** the Ollama app auto-updated but the currently-running server process is still the old
binary (or vice versa for a CLI installed separately via Homebrew). `cargo run --release -- doctor`
surfaces this explicitly. Fix: fully quit and relaunch the Ollama app (`osascript -e 'quit app
"Ollama"'` then `open -a Ollama`, or use `benchmark/local/scripts/restart_ollama.sh` as a reference).

## A background benchmark/proxy process outlived its intended lifetime and raced a later run

**Cause:** a long-running background job wasn't explicitly stopped before starting a new one
pointed at the same output directory — both wrote into `results/<model>/` concurrently, and
`aggregate.py`'s "latest complete run per model" logic got confused by interleaved partial runs.
Fix: before launching any run against a shared results directory, check for stragglers first:
```bash
ps aux | grep -E "run_matrix|run\.py|octocode_agent|bash_agent" | grep -v grep
```
Kill anything unexpected, `rm -rf` the results directory for a truly clean slate, and only then
launch. Never assume a background task you didn't just observe finishing is actually gone.
