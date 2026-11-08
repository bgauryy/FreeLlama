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

**Check it:** discard `verification.recommendation:"escalate"`, and verify the citations otherwise. Measured here it was often right but also wrong with confidence, so treat answers as leads.
