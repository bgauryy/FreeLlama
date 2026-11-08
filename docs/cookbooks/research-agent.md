# Ask a local agent to research a repository

**Use when** a question would take you several searches and reads, and the answer is cheap to check: where is X, what calls Y, which PR added Z.

**Skip it when** one search answers it, or when the answer needs your judgment.

**Model:** `bonsai-2:9b` (3.2 GB), needing about 8.6 GB available.

Local code, using the installed Octocode CLI's read-only tools. Pass the project's absolute path; a relative one resolves against the server's working directory, which is often not your project. The path must be under `FREELLAMA_MCP_ALLOWED_ROOTS`, which a published install requires:

```json
{"tool": "delegate_research", "arguments": {"question": "Which files call the function memory_holders?", "workspacePath": "/Users/me/code/my-project", "adapter": "octocode", "model": "bonsai-2:9b"}}
```

GitHub, without cloning or a `workspacePath`:

```json
{"tool": "delegate_research", "arguments": {"question": "Which file in ollama/ollama defines SystemOneRequest?", "adapter": "github", "model": "bonsai-2:9b"}}
```

The agent's instructions are [local](../octocode/LOCAL.md) and [GitHub](../octocode/GITHUB.md), which also work for any agent using the CLI directly.

With `adapter:"bash"` (the default) the model gets structured `search`, `read` and `list` actions that FreeLlama runs correctly and confined (a literal search by default; a path written relative to a package is resolved; a regex sent as a literal is retried as one), plus a read-only shell for `rg`, `grep`, `find`, `sed -n` and `git`. The GitHub adapter also takes the GitHub CLI read-only (`gh search`, `gh repo view`, `gh pr view`, `gh api` GET) through your existing `gh` login.

Every run starts with a short workspace overview (top level, file types, source folders). An answer that cites a file that does not exist, or names no source although its results did, gets one repair turn; an answer that still cites a missing file is marked unverified. Bigger models get more time (180 s up to 4 GB, then 20 s per GB); pass `agent.timeoutSeconds` to choose.

**Check it:** discard `verification.recommendation:"escalate"`, and verify the citations otherwise. Measured here it was often right but also wrong with confidence, so treat answers as leads.
