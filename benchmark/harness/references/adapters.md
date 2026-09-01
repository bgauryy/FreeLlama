# Agent Adapters

Load when connecting a CLI or agent harness. Why: normalization makes unlike agents comparable.

`run.py` expands `{model}`, `{prompt_file}`, `{workspace}`, and `{result_file}` in `--agent-command`, and replaces `__REPO_ROOT__` with this checkout (needed because the adapter's cwd is the disposable workspace, not the repo). It also exports those values as `FREELLAMA_BENCH_MODEL`, `FREELLAMA_BENCH_PROMPT`, `FREELLAMA_BENCH_WORKSPACE`, and `FREELLAMA_AGENT_RESULT`.

An adapter may write this JSON to `{result_file}`:

```json
{
  "final_answer": "...",
  "tool_calls": [{"name":"read","arguments":{"path":"src/x.py"},"status":"ok","duration_ms":12}],
  "usage": {"input_tokens":1200,"output_tokens":180,"cache_read_tokens":800,"cache_write_tokens":0},
  "provider_metrics": {"load_ms":90,"prompt_tokens_per_second":500,"decode_tokens_per_second":40}
}
```

Normalize tool names to capabilities: `search`, `read`, `edit`, `shell`, `test`, `skill.load`, or `mcp.<server>.<tool>`. Preserve raw names under `raw_name` if useful. If no result file exists, stdout is accepted but usage and trajectory scores remain unavailable.

The command must be non-interactive and exit nonzero on agent failure. Put provider-specific prompting, authentication, and MCP configuration in the adapter, not the suite.

Next: run the suite using the RUN route in `SKILL.md`.

