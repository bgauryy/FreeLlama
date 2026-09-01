# Context-management evidence

Ollama's stable API reports `prompt_eval_count` in the completed generate/chat response, so it can
calibrate later calls but cannot count a first prompt before execution. The upstream OpenAPI schema
has no stable `/api/tokenize`; the proposed endpoint remains an open pull request and its own notes
call it experimental. Sources:

- [Ollama usage metrics](https://docs.ollama.com/api/usage)
- [Ollama OpenAPI schema](https://github.com/ollama/ollama/blob/main/docs/openapi.yaml)
- [Upstream tokenize/detokenize proposal](https://github.com/ollama/ollama/pull/12030)

FreeLlama therefore uses a typed first-call character estimate and raises its multiplier from real
`prompt_eval_count` samples. It never lowers the multiplier. Model-specific calibration is stored
in `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR`, reused by later adapter processes, and never crosses
model templates. The records contain no prompt text. Default pinned overflow is `error`, which
preserves the system prompt and original question byte-for-byte; `clip` is opt-in.

Reproduce the deterministic boundary:

```bash
python3 benchmark/local/scripts/test_agent_context.py  # 65 contracts
python3 benchmark/local/scripts/test_agent_actions.py  # both adapter action schemas
```

The MCP E2E behavior test sends non-default `charsPerToken` and `observationPageChars` through
`delegate_research.agent`, performs a real model-backed lookup, and asserts the same values plus a
positive calibration count in `contextManagement`.
