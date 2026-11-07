# Find the code for an issue or question

**Use when** you do not know which file to read and have no exact word to search for. The server embeds the files and returns ranked `path:Lstart-Lend` labels, so you read a few ranges instead of a repository.

**Skip it when** you know an identifier: `localSearch` or `grep` is faster and exact.

**Model:** `embeddinggemma-2:740m-q8_0` (1.3 GB). `nomic-embed-text:v1.5` (0.3 GB) indexes faster but found the right file less often.

```json
{"tool": "run_task", "arguments": {"task": "embedding", "model": "embeddinggemma-2:740m-q8_0", "files": ["src/**/*.rs"], "nearest": {"query": "task: search result | query: why does saving a long path crash", "top": 5, "documentPrefix": "title: none | text: "}}}
```

The prefixes belong to the model: `task: search result | query: ` and `title: none | text: ` for embeddinggemma; `search_query: ` and `search_document: ` for nomic. Other models have their own; check the model's page.

CLI, for items one per line: `freellama task --task embedding --model embeddinggemma-2:740m-q8_0 --input-file items.txt --nearest "task: search result | query: saving crashes" --top 5`

**Read the result:** one line per file, best chunk first, with a cosine score.

**Check it:** read the top ranges before you rely on them. Measured here on 10 questions about this repository: embeddinggemma put the right file first for 9 and in the top 3 for 9; nomic, 6 and 8. A miss is still normal; widen `top` or rephrase before you conclude anything.

Each call embeds the files again, so narrow the glob. A glob over about 900 chunks took about 67 s with embeddinggemma and 42 s with nomic.
