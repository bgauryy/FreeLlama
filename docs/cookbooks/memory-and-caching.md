# Keep local work light: memory, context and caching

**See what fits before you start:**

```json
{"tool": "models", "arguments": {"view": "catalog", "job": "classify"}}
```

```json
{"tool": "doctor", "arguments": {"view": "status"}}
```

**Load before a burst.** This loads the model and evaluates a shared prompt once, so later calls that send the same prefix skip that work. It saves local time, not your tokens: you still send the prefix.

```json
{"tool": "ollama_manage", "arguments": {"action": "warm", "model": "bonsai-2:9b", "systemPrompt": "You review release notes.", "contextTokens": 8192}}
```

**Free memory when a burst ends.** Unload a model you warmed, or one the user named. It waits for that model's running tasks.

```json
{"tool": "ollama_manage", "arguments": {"action": "unload", "model": "bonsai-2:9b"}}
```

**Caching.** Reuse needs the same text at the start of the prompt, so put stable text first and append rather than edit. `metrics.cached_prompt_tokens` shows what was reused. Measured here on `gpt-oss:20b`: 1,251 of 1,266 tokens reused, with prompt evaluation falling from 544 ms to under 40 ms.

**Context window.** `contextTokens` is input plus output. A larger window uses more memory and can reload the model, so ask for what the prompt needs. FreeLlama never cuts a prompt silently.

**When a load is refused,** the message says how much memory is available and needed. Tell the user, and never stop their processes. Retry once they free memory, or use a smaller installed model with the same capability; do not retry in a loop.

Each pick's memory need is `memory_gb` in `models {view:"catalog"}`; measured models and their speeds are in [Choose local models](../MODEL_SELECTION.md).
