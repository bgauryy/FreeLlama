# Offload small coding tasks

**Use when** the task is small and self-contained, and you can check the answer cheaply: explain a function, write a regex, convert a snippet, draft a test.

**Skip it when** the change spans files, needs judgment you must stand behind, or is shorter than describing it. Local code review is unreliable: verify every finding.

**Model:** `lfm3:1.7b-q4_K_M` (1.1 GB, about 3.3 GB available needed). Measured here on 25 small questions over three rounds: lfm3 61 right, `bonsai-2:9b` 55, and Nemotron-3.5-Bolt 68, which needs about 13.5 GB available. Run `models {view:"catalog", job:"coding"}` for what fits now.

Pass the code by path:

```json
{"tool": "run_task", "arguments": {"task": "coding", "model": "lfm3:1.7b-q4_K_M", "prompt": "Write a unit test for the function in this file. Reply with code only.", "files": ["src/parse.ts"], "options": {"num_predict": 800}}}
```

CLI: `freellama task --task coding --model lfm3:1.7b-q4_K_M --input-file prompt.txt`

**Check it:** run the code. Set `num_predict` high enough for a thinking model's reasoning, which counts as output, or pass `think:false` for a short answer: an answer that comes back empty says which.
