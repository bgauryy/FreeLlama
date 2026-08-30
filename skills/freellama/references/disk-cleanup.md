# Disk cleanup: what's automated, what isn't, and why

Load when deleting models, or when changing anything about workspace/fixture retention.

## The incident this is based on

A benchmark session filled a 926GB disk to 100% (188Mi free) while running local-model comparisons.
Root causes, in order of contribution:

1. **~111GB of installed Ollama models**, several genuine duplicates (three separate tags —
   `gemma4:12b-mlx`, `gemma4:12b`, `gemma4:latest` — all pointing at essentially the same model),
   several unused for 6-8 months.
2. **The benchmark harness's own workspace accumulation.** Each trial copies the full fixture repo
   set (~600MB: click + zustand + openui) into a fresh disposable workspace, and with 30 questions
   × 2 agents × several re-runs (including some killed/restarted mid-flight), this piled up to
   15-16GB per completed run, never cleaned up automatically.

It got bad enough that `df` showed 188Mi free and even small command-output writes started failing
with `ENOSPC` — a genuinely disruptive failure mode, not just an inconvenience.

## What's fixed (automated, safe, no judgment call required)

- **`benchmark/local/scripts/run_all.sh` now always passes `--discard-workspaces`** to
  `run_matrix.py`. This keeps the graded artifacts (`prompt.md`, `stdout.txt`, `stderr.txt`,
  `agent-result.json`, `trial-N.json`, `aggregate.json`, `index.html`) and deletes the disposable
  full-repo workspace copy per trial. There is no scenario in this benchmark's design where keeping
  the post-grading workspace copy is needed — this was purely fixing our own code's behavior, no
  risk, no downside. This is why it's a default, not a flag you have to remember.
- **`scripts/check.sh` now warns on low absolute free disk space** (default threshold 15GB, override
  with `MIN_FREE_GB=<n>`) — so a tightening disk shows up as a WARN/FAIL before commands start
  failing, not after. Threshold is absolute GB, not percent-used: a dev machine sitting at 90%+ full
  from unrelated data is normal and shouldn't nag; the incident above was about genuinely low
  absolute headroom.
- **`scripts/check.sh` now reports installed models untouched for 3+ months** as an informational
  list — same "have I used this recently?" heuristic the wider Ollama community recommends for
  manual quarterly review (see sources below).

## What's deliberately NOT automated, and why

**Automated model deletion was considered and rejected.** A staleness heuristic ("not modified in
N months → delete") is a real hazard: a model can sit untouched for days or weeks and then become
exactly what a new task needs (this happened in this very session — `muse-glimmer:30b-mlx` sat idle
before being needed for a 3-model comparison). Deleting it automatically based on age alone would
have been actively harmful, not helpful. This mirrors the retry-budget/circuit-breaker decision in
`references/reliability.md`: the instinct to automate is worth resisting when the risk of a wrong
automated decision (re-downloading tens of GB, or losing something the user meant to keep) is much
higher than the cost of a human glancing at a report and deciding.

Industry practice agrees: the recommended Ollama cleanup workflow is manual review
("`ollama list`, sorted by modification date, ask 'have I used this in 3 months?'" — or the
`models` MCP tool (`view: "raw"`) if an agent is connected), not automation — see sources. `ollama rm`
(or the `ollama_delete` MCP tool) is explicitly the *only* safe deletion path; the model blob store
under `$HOME/.ollama/models` is content-addressed and manifest-tracked, so deleting files there
directly (bypassing `ollama rm`/`ollama_delete`) can corrupt the manifest. `ollama_delete`'s own
tool description enforces the same rule stated above: only on an explicit human instruction naming
the exact model, never on an automated staleness heuristic. Nothing in this skill ever touches that
directory directly, and it never will.

**Ollama's own disk-backed prompt cache** (`~/.ollama` server logs show `"prompt cache is enabled,
size limit: 8192 MiB"`) was investigated only as far as confirming it's self-capped by Ollama itself
— it wasn't a contributor to this incident and doesn't need separate management here. If it ever
is implicated in a future disk issue, that's a new investigation, not an assumption to build on now.

## Sources

- [Best Ways to Manage Multiple Ollama Models: 2026 Workflows](https://insiderllm.com/guides/managing-multiple-models-ollama/)
- [Clear Ollama Model Cache: Complete Storage Management Guide](https://markaicode.com/clear-ollama-model-cache-storage-guide/)
- [How to Remove Unused Ollama Models on Mac](https://devcleaner.app/guides/remove-unused-ollama-models)
- [Local AI Models Eating Your Disk: Ollama, LM Studio](https://1erkinyagci.github.io/maccleaner/blog/local-ai-models-disk-space-mac.html)
