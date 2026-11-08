# Let FreeLlama pick the model and prompt (job mode)

**Use when** one of the jobs below fits the work and you would rather not write the local model's prompt, options and embedding prefixes yourself. Pass `job` to `run_task`: FreeLlama runs the job's first catalog pick that is installed and fits now, with its own prompt for that job. Any field you pass wins, and `job_profile.applied` in the result lists what job mode filled in.

**Skip it when** you need your own wording or decoding: pass `model`, `prompt` and `options` yourself, as the other recipes do.

| `job` | Task | First pick | What job mode fills in |
|---|---|---|---|
| `classify` | decision | `strands-decider:2b-nvfp4` | task and model; you send `questions` |
| `find-code` | embedding | `embeddinggemma-2:740m-q8_0` | `task: code retrieval \| query: ` before your query, `title: none \| text: ` before each chunk |
| `duplicates` | embedding | `nomic-embed-text:v1.5` | `clustering: ` before every item; the most similar pairs when you send no `nearest` |
| `ocr` | vision | `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` | prompt `Text Recognition:`; send `Table Recognition:` or `Formula Recognition:` as `prompt` for those |
| `image-recognition` | vision | `lfm3-vl:2b-q4_K_M` | a brief-answer system prompt and the card's sampling; you send the question as `prompt` |
| `image-search` | embedding | `embeddinggemma-2:740m-q8_0` | `task: search result \| query: ` before your query; images go in bare |
| `coding` | coding | `lfm3:1.7b-q4_K_M` | a system prompt asking for SEARCH/REPLACE edits, and the card's sampling; you send the change as `prompt` |
| `summarize` | completion | `lfm3:1.7b-q4_K_M` | a short-bullets system prompt that quotes numbers and names and says "not stated" rather than guess |

The later picks of each job are the catalog's (`models {view:"catalog", job}`). Every value, with the model card it comes from, is in [local-prompts.ts](../../packages/mcp/src/local-prompts.ts); a sampling value is used only where the model's own card documents it.

Read the text in one image:

```json
{"tool": "run_task", "arguments": {"job": "ocr", "files": ["screenshots/error.png"]}}
```

One answer per image, with the job sent once:

```json
{"tool": "run_task", "arguments": {"job": "ocr", "tasks": [{"id": "a", "independent": true, "task": {"files": ["screenshots/a.png"]}}, {"id": "b", "independent": true, "task": {"files": ["screenshots/b.png"]}}]}}
```

Find the code for an issue:

```json
{"tool": "run_task", "arguments": {"job": "find-code", "files": ["src/**/*.rs"], "nearest": {"query": "why does saving a long path crash", "top": 5}}}
```

Draft one small change:

```json
{"tool": "run_task", "arguments": {"job": "coding", "files": ["src/parse.ts"], "prompt": "Make parsePort reject ports above 65535."}}
```

**Apply coding edits yourself.** The answer is blocks of a path line, `<<<<<<< SEARCH`, the current lines, `=======`, the replacement and `>>>>>>> REPLACE`, or a path and one fenced block for a whole new file. Check that each SEARCH text occurs exactly once in its file, indentation included, replace it, skip a block that matches zero or several times, then run the tests.

**Hand a result to the next task.** `saveAs` writes the answer to `handoffs/<name>.md` in the FreeLlama home, with a `## Summary` and a `## Result`, and returns its path as `handoff_file`. Another task reads it with `files`, so you never paste it back:

```json
{"tool": "run_task", "arguments": {"job": "summarize", "files": ["logs/build.log"], "prompt": "Which steps failed, and why?", "saveAs": "build-failures"}}
```

```json
{"tool": "run_task", "arguments": {"job": "coding", "files": ["/Users/me/.octocode/freellama/handoffs/build-failures.md", "scripts/build.mjs"], "prompt": "Fix the first failure the summary names."}}
```

**When nothing fits now,** the refusal lists each pick, the memory it needs and what is free. Tell the user; pass `model` for an installed model with the same capability, or ask them to approve pulling one exact tag at its size.

**MCP prompts.** Clients that show prompts (often as slash commands) list one per job: `ocr`, `image`, `classify`, `find-code`, `duplicates`, `coding`, `summarize`, `research`, `github-research` and `sweep`. Each returns the exact call to make; `freellama` explains when to offload and how to check results.

**Check it:** as the job's own recipe says; read 3 to 5 results before relying on the rest.
