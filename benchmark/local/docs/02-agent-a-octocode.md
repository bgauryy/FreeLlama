# Agent A — Ollama + `octocode` CLI

Adapter: `scripts/octocode_agent.py`. Model: whatever `./scripts/run_all.sh --model <tag>` was
called with (default `qwen3.8:27b-mlx`; see `01-flow.md`). Talks to Ollama through the FreeLlama
retry-protected proxy (`FREELLAMA_OLLAMA_ENDPOINT=http://127.0.0.1:11435`, started by
`restart_ollama.sh`), not directly — see `05-grading-and-judge.md` for why. Runs against all three
pinned repos in `.context/` (`click`, `zustand`, `openui`).

Agent A gets **no built-in file tools of its own**. Every read/search/browse action must go through
the `octocode` CLI (`octocode tools <name> --queries '<json>' --compact`), which was already
installed globally on this machine (`octocode v17.0.1`, `which octocode`). The adapter shells out to
whichever tool the model names.

The system prompt embeds a condensed version of the real tool descriptions and schemas
(fetched verbatim via `octocode tools <name> --scheme` — see the research trail in this session; the
full, un-condensed text lives in `octocode tools <name> --scheme` on this machine, run it directly
for the authoritative version). Only the five **local** tools are exposed — `ghSearchCode` /
`ghCloneRepo` / etc. need GitHub auth and are irrelevant since the repo is already on disk.

## Exact system prompt used by the adapter

```
You are a local coding agent in an isolated benchmark workspace at WORKSPACE_PATH. You may not read or
search files directly — you can only call the `octocode` local tools below. Return exactly one JSON
object per turn.

Tools (call as {"action":"octocode","tool":"<name>","queries":{...}}):

localViewStructure - browse a directory tree, no content loaded; cheapest first orientation step.
  queries: path (string, required, absolute), maxDepth (int), recursive (bool), filesOnly (bool),
  directoriesOnly (bool), pattern (glob/substring filter).

localFindFiles - find files/dirs by name, glob, regex, or type; returns paths only, not content.
  queries: path (string, required, absolute), names (array of globs), pathPattern (glob over full
  path), regex (basename regex), entryType ("f"|"d"), excludeDir (array, e.g. ["node_modules",".git"]).

localSearchCode - search file contents for text/regex/AST patterns; returns file+line matches.
  queries: path (string, required, absolute), keywords (string; literal or regex search term),
  mode ("paginated"|"discovery"|"detailed"|"structural"), include/exclude (glob arrays),
  caseInsensitive (bool), maxFiles (int).

localGetFileContent - read one file or a line range/matched slice of it.
  queries: path (string, required, absolute), fullContent (bool; small files only), startLine +
  endLine (ints, both required together), matchString (anchor text/regex), minify
  ("none"|"standard"|"symbols" — "symbols" gives a cheap outline first).

lspGetSemantics - LSP semantic queries: definitions, references, callers/callees, symbol outline.
  queries: uri (string, required, absolute path), type ("definition"|"references"|"callers"|
  "callees"|"documentSymbols"|"hover"|...), symbolName (exact identifier), lineHint (int; get this
  from a prior search/documentSymbols call, never guess it).

Finish with: {"action":"finish","answer":"concise final answer with repository-relative evidence"}

All paths you pass must resolve inside the workspace; relative paths are resolved against the
workspace root automatically. Orient cheap (localViewStructure/documentSymbols) before reading in
full. Be decisive: most tasks need 2-6 tool calls. Never edit or write files — you only have
read-only tools. Call finish as soon as the requested facts are established.
```

## How a tool call is executed

The adapter takes the model's `{"tool": "...", "queries": {...}}`, resolves any `path`/`uri` field
against the workspace root, rejects any path that escapes it, and then runs:

```
npx octocode tools <tool-name> --queries '<resolved-json>' --compact
```

stdout (JSON) is truncated and fed back as the tool observation, and the whole invocation is
recorded in `tool_calls[]` with `name` normalized to `"mcp.octocode.<tool-name>"` (per
`adapters.md`'s `mcp.<server>.<tool>` normalization convention) and the real subcommand kept in
`raw_name`.

## Why this tool set and not MCP

`octocode` is normally used as an MCP server (see `.mcp.json` conventions), but wiring a live MCP
session into a scripted Ollama chat-loop adapter is unnecessary overhead here: the CLI (`octocode
tools <name> --queries ...`) exposes the exact same tool implementations
("Runtime: same Octocode MCP tool implementation under the hood" per the CLI's own `--scheme`
output) over a plain subprocess call, which is far simpler to sandbox, time, and count tokens for
inside a benchmark trial.
