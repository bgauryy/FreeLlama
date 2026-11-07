# Find an image by describing it

**Use when** you have a folder of screenshots or images and need the one that matches a description.

**Model:** `embeddinggemma-2:740m-q8_0` (1.3 GB). Its text-only tags do not embed images.

```json
{"tool": "run_task", "arguments": {"task": "embedding", "model": "embeddinggemma-2:740m-q8_0", "files": ["screenshots/*.png"], "nearest": {"query": "task: search result | query: a compiler error in a terminal", "top": 3}}}
```

Image files are embedded whole, and each is one item. WAV files work the same way with a model that has audio input, such as the same model; audio has not been measured here.

**Check it:** open the top match. Measured here: 5 of 5 on five drawn screens, too few to say more.
