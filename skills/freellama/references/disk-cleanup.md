# Safe disk cleanup

Load when disk space is tight or an installed model looks unused. Why: Ollama's blob store is
content-addressed and direct file deletion can corrupt its manifests.

1. Run `scripts/check.sh`; the default hard warning is less than 15 GB free.
2. Treat old-model output as a candidate list, never an automatic retention policy.
3. Ask a human to name the exact tag to delete in the current conversation.
4. Delete only with `ollama rm <tag>` or `ollama_delete`.

Never delete files under `~/.ollama/models`, and never infer consent from age. Workspace copies and
benchmark outputs can also consume many gigabytes; resolve their exact paths before cleanup.

Measured incident details and sources: `assets/evidence/disk-cleanup.md`.

Next: memory rather than disk → `references/ollama-config.md`.
