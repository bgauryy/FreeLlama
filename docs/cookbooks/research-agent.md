# Ask a local agent to research a repository

**Use when** a question would take you several searches and reads, and the answer is cheap to check: where is X, what calls Y, which PR added Z.

**Skip it when** one search answers it, or when the answer needs your judgment.

**Explicit example model:** `bonsai-2:9b` (3.2 GB file). Inspect current residency and the route
assessment; disk size and historical memory previews do not establish fit. Without `model`, this
tool checks the configured default, then installed research catalog picks, then routing. This is
separate from the [calibrated-first job/workflow path](../MODEL_SELECTION.md#turn-measurements-into-routing-evidence).

Local code, using the installed Octocode CLI's read-only tools. Pass the project's absolute path; a relative one resolves against the server's working directory, which is often not your project. The path must be under `FREELLAMA_MCP_ALLOWED_ROOTS`, which a published install requires:

```json
{"tool": "delegate_research", "arguments": {"question": "Which files call the function memory_holders?", "workspacePath": "/Users/me/code/my-project", "adapter": "octocode", "model": "bonsai-2:9b"}}
```

GitHub, without cloning or a `workspacePath`:

```json
{"tool": "delegate_research", "arguments": {"question": "Which file in ollama/ollama defines SystemOneRequest?", "adapter": "github", "model": "bonsai-2:9b"}}
```

The read-only agent's instructions are [local](../octocode/LOCAL.md) and [GitHub](../octocode/GITHUB.md), which also work for any agent using the CLI directly.

With `adapter:"bash"` (the default) the model gets structured `search`, `read` and `list` actions that FreeLlama runs correctly and confined (a literal search by default; a path written relative to a package is resolved; a regex sent as a literal is retried as one), plus a read-only shell for `rg`, `grep`, `find`, `sed -n` and `git`. The GitHub adapter also takes the GitHub CLI read-only (`gh search`, `gh repo view`, `gh pr view`, `gh api` GET) through your existing `gh` login.

Every run starts with a short workspace overview (top level, file types, source folders). An answer that cites a file that does not exist, or names no source although its results did, gets one repair turn; an answer that still cites a missing file is marked unverified. Bigger models get more time (180 s up to 4 GB, then 20 s per GB); pass `agent.timeoutSeconds` to choose.

The caller owns decomposition, prompts, dependencies, final judgment and mutation authority.
The writing exception is `adapter:"coder"`, disabled unless the operator sets
`FREELLAMA_MCP_ALLOW_CODER=1`. It edits an isolated worktree or copy and runs only the exact
commands supplied in `agent.commands`; it returns a candidate patch and never applies it to the
caller's tree. Inspect `diff`, `changedFiles`, `runs` and `runSandbox`, then verify the
patch before deliberately applying it. Isolation and an allowlisted command do not prove patch
correctness or make an unsafe command safe. See the [coder contract](../../packages/mcp/src/tools/coder.ts).

Every attempted allowed command retains raw combined stdout/stderr in a private file, including
runs whose inline preview is no longer shown. `runs[].output_audit` records the path, metadata
path, bytes, SHA-256, byte cap, completion state and reason; `runs[].failed` also covers a nonzero
exit. Ordering means observed pipe-chunk arrival, not a perfect cross-stream interleaving.
The model reads bounded head/tail previews through the existing observation paging; the caller
can inspect the full raw file. Previews do not imply the raw middle was discarded.

The default cap is 16 MiB per command; the operator may set
`FREELLAMA_CODER_MAX_OUTPUT_BYTES` (maximum 256 MiB). A cap or audit-write failure terminates the
command, marks its output incomplete and returns a tool-error observation. Timeouts and pipes
still open after shell exit are also explicitly incomplete. If an adapter is interrupted before
it writes its result, `commandAuditDirectory` still locates retained logs; a sidecar written
before execution stays incomplete until finalized. This is normal-process retention, not a
power-loss durability guarantee. Logs survive isolated-copy cleanup, even when no patch exists;
retain them with the receipt or remove their private temporary directory after review.
Custom log directories must be outside the caller workspace, the entire disposable copy container
and every command-writable directory. Per-command caps do not impose a quota on retained logs
across multiple sessions.

**Check it:** discard `verification.recommendation:"escalate"`, and verify the citations otherwise. Measured here it was often right but also wrong with confidence, so treat answers as leads.
