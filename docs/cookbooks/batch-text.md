# Rewrite, summarize or extract across many texts

**Use when** you have enough similar short texts that sharing their instructions saves work. Send what they share once, and name each input by path.

**Skip it when** a pattern or script can do it exactly: checking a format with a regex beats any model.

**Model:** run `models {view:"catalog", job:"coding"}`; the same chat models apply.

```json
{"tool": "run_task", "arguments": {"task": "completion", "model": "bonsai-2:9b", "systemPrompt": "Summarize this release note in one line.", "options": {"num_predict": 120}, "tasks": [{"id": "v1", "independent": true, "task": {"prompt": "Release note:", "files": ["releases/v1.md"]}}, {"id": "v2", "independent": true, "task": {"prompt": "Release note:", "files": ["releases/v2.md"]}}]}}
```

Up to 64 items per call. Top-level `task`, `model`, `systemPrompt`, `questions`, `format` and `options` apply to every item; `prompt`, `state`, `input`, `files` and `images` stay per item.

`job:"summarize"` in place of `model` and `systemPrompt` lets FreeLlama pick the model and its summary prompt ([jobs](jobs.md)). On a single call, `saveAs` writes the answer to a handoff file a later task reads with `files`.

**Read the result:** one `## <id>` section per item, in order.

**Check it:** verify representative and difficult items against their sources, and every item
whose error would matter. Read the [check-line limits](../RELIABILITY.md#accuracy-of-result-checks);
a clean lexical check does not establish summary correctness.
