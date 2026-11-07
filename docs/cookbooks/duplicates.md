# Find duplicates among issues or texts

**Use when** you need likely duplicate pairs from many short texts.

**Model:** `nomic-embed-text:v1.5` (0.3 GB).

Leave out `query`, and `nearest` returns the most similar pairs:

```json
{"tool": "run_task", "arguments": {"task": "embedding", "model": "nomic-embed-text:v1.5", "files": ["issues/*.md"], "nearest": {"top": 10, "documentPrefix": "search_document: "}}}
```

Two chunks of one file are never reported as a pair.

**Read the result:** one line per pair, with a cosine score.

**Check it:** a high score is a candidate, never a verdict. Read both before you close anything. Measured here: 8 of 12 real related issues found their partner as the nearest neighbour.
