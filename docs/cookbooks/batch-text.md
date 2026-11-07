# Rewrite, summarize or extract across many texts

**Use when** you have 10 or more similar short texts. Send what they share once, and name each input by path.

**Skip it when** a pattern or script can do it exactly: checking a format with a regex beats any model.

**Model:** run `models {view:"catalog", job:"coding"}`; the same chat models apply.

```json
{"tool": "run_task", "arguments": {"task": "completion", "model": "bonsai-2:9b", "systemPrompt": "Summarize this release note in one line.", "options": {"num_predict": 120}, "tasks": [{"id": "v1", "independent": true, "task": {"prompt": "Release note:", "files": ["releases/v1.md"]}}, {"id": "v2", "independent": true, "task": {"prompt": "Release note:", "files": ["releases/v2.md"]}}]}}
```

Up to 64 items per call. Top-level `task`, `model`, `systemPrompt`, `questions`, `format` and `options` apply to every item; `prompt`, `state`, `input`, `files` and `images` stay per item.

**Read the result:** one `## <id>` section per item, in order.

**Check it:** read 3 to 5, more where a mistake costs more.
