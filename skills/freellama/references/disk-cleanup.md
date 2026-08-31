# Disk cleanup: what may be deleted, and what must never be

Load when the disk is tight, when a model looks like a deletion candidate, or before automating any
retention behaviour.

## The incident behind this policy

A benchmark session filled a 926GB disk down to 188Mi free; even small writes failed with `ENOSPC`.
Causes, in order: **~111GB of installed Ollama models** (duplicate tags of the same model, several
unused for months) and **workspace accumulation** — each trial copied ~600MB of fixtures, and 30
questions × 2 agents × re-runs piled up 15-16GB per run that nothing cleaned.

## The one safe deletion path

`ollama rm <model>` — or the `ollama_delete` MCP tool — and nothing else. The blob store under
`~/.ollama/models` is content-addressed and manifest-tracked, so deleting files there directly can
corrupt the manifest. Nothing in this skill ever touches that directory, and it never will.

`ollama_delete` carries `destructiveHint: true` so a client can gate it without parsing prose, and
it must only be called after a human has named that exact model for deletion in the current
conversation.

## Never automate deletion on a staleness heuristic

"Not modified in N months → delete" is a real hazard: a model can sit untouched for weeks and then
be exactly what a new task needs. That happened here — `muse-glimmer:30b-mlx` sat idle before being
needed for a three-model comparison. Age-based deletion would have been actively harmful.

This mirrors the retry-budget decision in `references/reliability.md`: the instinct to automate is
worth resisting when a wrong automated decision (re-downloading tens of GB, or losing something the
user meant to keep) costs far more than a human glancing at a report. Industry practice agrees — the
recommended Ollama workflow is manual quarterly review, not automation (sources below).

**So: report candidates, let a human decide.** `scripts/check.sh` does exactly that:

- warns on low **absolute** free space (default 15GB, override `MIN_FREE_GB=<n>`), so a tightening
  disk surfaces before commands start failing. Absolute rather than percent-used on purpose: a dev
  machine sitting at 90% full from unrelated data is normal and should not nag.
- lists installed models untouched for 3+ months as **informational only** — the same "have I used
  this recently?" prompt the wider Ollama community recommends for manual review.
- never deletes anything, restarts anything, or mutates state. Read-only by construction.

`models {view:"raw"}` gives an agent the same estate listing when one is connected.

## Not a factor: Ollama's own prompt cache

Ollama's disk-backed prompt cache self-caps (its server log states `"prompt cache is enabled, size
limit: 8192 MiB"`). It did not contribute to the incident above and needs no management here. If it
is ever implicated, that is a new investigation, not an assumption to build on now.

Next: freeing *memory* rather than disk → `references/ollama-config.md`. Deciding which model to keep
in the first place → `references/model-selection.md`.

## Sources

- [Best Ways to Manage Multiple Ollama Models: 2026 Workflows](https://insiderllm.com/guides/managing-multiple-models-ollama/)
- [Clear Ollama Model Cache: Complete Storage Management Guide](https://markaicode.com/clear-ollama-model-cache-storage-guide/)
- [How to Remove Unused Ollama Models on Mac](https://devcleaner.app/guides/remove-unused-ollama-models)
- [Local AI Models Eating Your Disk: Ollama, LM Studio](https://1erkinyagci.github.io/maccleaner/blog/local-ai-models-disk-space-mac.html)
