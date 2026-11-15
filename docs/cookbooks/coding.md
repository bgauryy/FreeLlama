# Offload small coding tasks

**Use when** the task is small and self-contained, and you can check the answer cheaply: explain a function, write a regex, convert a snippet, draft a test.

**Skip it when** the change spans files, needs judgment you must stand behind, or is shorter than describing it. Local code review is unreliable: verify every finding.

**Historical starting point:** `bonsai-2:9b` (3.2 GB file; the dated catalog preview asked for
10.3 GB available). It answered every case in one 2026-11-12 field-test round, with SEARCH/REPLACE
edits and tool calls; that result does not establish general coding or tool reliability.
Nemotron-3.5-Bolt scored best on 25 small questions (68 right over three rounds, against 55) but its
preview asked for 13.5 GB available. `lfm3:1.7b` scored 61 on the questions, then answered workflows
with tool calls nobody asked for, so it was removed from the portable picks.

Inspect `models {view:"catalog", job:"coding"}` for current candidates. Workflows without `model`
use [local calibration before fallback](../MODEL_SELECTION.md#turn-measurements-into-routing-evidence);
the [newer tool-calling smoke results](../RELIABILITY.md#measured-on-2026-11-14) are separate evidence.

Pass the code by path:

```json
{"tool": "run_task", "arguments": {"task": "coding", "model": "bonsai-2:9b", "prompt": "Write a unit test for the function in this file. Reply with code only.", "files": ["src/parse.ts"], "options": {"num_predict": 800}}}
```

CLI: `freellama run -w write-tests -f src/parse.ts` (FreeLlama picks the model), or `freellama run -m bonsai-2:9b -f src/parse.ts "Write a unit test" --think=false`

**Check it:** run the code. A reply that is a tool call although you sent no tools is not an answer: the CLI exits 1 on it; rerun on a stronger installed pick. Set `num_predict` high enough for a thinking model's reasoning, which counts as output, or pass `think:false` for a short answer: an answer that comes back empty says which.
