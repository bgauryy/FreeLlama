# Agent B — Ollama + raw Linux shell only

Adapter: `scripts/bash_agent.py`. Same model (`FREELLAMA_TARGET_MODEL`), same decoding settings, same
turn budget, and same retry-protected proxy endpoint (`FREELLAMA_OLLAMA_ENDPOINT`) as Agent A — the
only difference under test is the tool surface.

Agent B gets no structured tool schema at all. It must solve every question by emitting one raw
POSIX shell command per turn (`ls`, `find`, `grep`, `cat`, `sed -n`, `awk`, `wc`, `head`, `tail`,
etc. — whatever is on `$PATH`), executed with `cwd` set to the disposable workspace copy (which
contains all three pinned repos: `click/`, `zustand/`, `openui/`).

## Exact system prompt used by the adapter

```
You are a local coding agent in an isolated benchmark workspace containing a pinned `click/`
repository, rooted at the current directory. You solve tasks using ONLY raw POSIX shell commands —
no editors, no special tools, no network access. Return exactly one JSON object per turn:

{"action":"shell","command":"one shell command, e.g. grep -n \"class Group\" click/src/click/core.py"}
{"action":"finish","answer":"concise final answer with repository-relative evidence"}

Use standard Unix utilities: ls, find, cat, grep, sed, awk, head, tail, wc, tree (if present). Chain
with pipes if needed, but keep each turn to a single shell invocation. Never edit files. Be decisive:
most tasks need 2-6 commands. Call finish as soon as the requested facts are established.
```

## How a command is executed

The adapter runs the model's `command` via `subprocess.run(["/bin/bash", "-c", command], cwd=workspace,
timeout=30, capture_output=True, text=True)` and feeds back combined stdout+stderr (truncated), same
as Agent A's tool observations. Every invocation is recorded in `tool_calls[]` with `name` normalized
to `"shell"` (the exact capability name `adapters.md` reserves for this) and the literal command kept
under `arguments.command`.

A short denylist blocks destructive patterns before execution (`sudo`, `rm -rf /`,
fork-bombs, `curl`/`wget`/`nc` for outbound network, redirection to device files) — not because the
model is expected to try these, but because an unconstrained shell has no other safety net. Contracts:
`scripts/test_bash_confine.py`. The workspace itself is a disposable per-trial copy, so anything the
command does to files inside it (including deleting them) is a legitimate, gradable outcome
(`no_changes` / `max_changed_files` checks fail that trial), not a safety incident.

## Why this is the fair baseline

This is deliberately the most widely available research tool: it is what any generic
shell-capable agent has access to with zero setup, and it is the natural point of comparison for
"does a purpose-built code-research tool (`octocode`) help a local model do code research
faster or more accurately, or does grep-and-cat perform as well?" Both agents share everything else (model,
decoding params, turn budget, workspace, questions, grading) so any measured difference in tokens,
tool-call count, wall time, or pass rate is attributable to the tool surface alone.
