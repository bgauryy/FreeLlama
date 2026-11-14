# Offload small coding tasks

**Use when** the task is small and self-contained, and you can check the answer cheaply: explain a function, write a regex, convert a snippet, draft a test.

**Skip it when** the change spans files, needs judgment you must stand behind, or is shorter than describing it. Local code review is unreliable: verify every finding.

**Model:** `bonsai-2:9b` (3.2 GB, about 10.3 GB available needed). It answered every workflow case in the 2026-11-12 field test, with proper SEARCH/REPLACE edits and correct tool calls. Nemotron-3.5-Bolt scored best on 25 small questions (68 right over three rounds, against 55) but needs about 13.5 GB available. `lfm3:1.7b` is no longer a pick: it scored 61 on the questions, then answered workflows with tool calls nobody asked for. Run `models {view:"catalog", job:"coding"}` for what fits now.

Pass the code by path:

```json
{"tool": "run_task", "arguments": {"task": "coding", "model": "bonsai-2:9b", "prompt": "Write a unit test for the function in this file. Reply with code only.", "files": ["src/parse.ts"], "options": {"num_predict": 800}}}
```

CLI: `freellama run -w write-tests -f src/parse.ts` (FreeLlama picks the model), or `freellama run -m bonsai-2:9b -f src/parse.ts "Write a unit test" --think=false`

**Check it:** run the code. A reply that is a tool call although you sent no tools is not an answer: the CLI exits 1 on it; rerun on a stronger installed pick. Set `num_predict` high enough for a thinking model's reasoning, which counts as output, or pass `think:false` for a short answer: an answer that comes back empty says which.
