# Classify, triage and yes/no checks

**Use when** you have many items to label, route or check against a rule. A decision model answers in one forward pass with a probability, and generates no text to parse.

**Skip it when** there is one item you already understand, or a wrong label would be costly and unchecked.

**Model:** `strands-decider:2b-nvfp4` (1.3 GB). See `models {view:"catalog", job:"classify"}` for what is installed and fits now.

Question types: `choice` picks one of the `criteria` labels; `noul` is a yes/no question, answered
as the probability of yes; `score` gives a level from 2 to 10 descriptions, lowest first.

From a shell, the same as `ollama run --ask`:

```bash
fllama run --ask "What kind of issue is this?" --choice bug="Something is broken" --choice feature="A request for new capability" \
  --ask "Does this need action today?" < issue.md
```

One item, with the question asked once:

```json
{"tool": "run_task", "arguments": {"task": "decision", "model": "strands-decider:2b-nvfp4", "state": "Checkout returns 500 since 9am.", "questions": {"kind": {"type": "choice", "instructions": "What kind of issue is this?", "criteria": {"bug": "Something is broken", "feature": "A request for new capability", "question": "A question"}}, "urgent": {"type": "noul", "instructions": "Does this need action today?"}}}}
```

Many items: send the question once and let each item carry only its own text or file:

```json
{"tool": "run_task", "arguments": {"task": "decision", "model": "strands-decider:2b-nvfp4", "questions": {"kind": {"type": "choice", "instructions": "What kind of issue is this?", "criteria": {"bug": "Something is broken", "feature": "A request for new capability"}}}, "tasks": [{"id": "a", "independent": true, "task": {"files": ["issues/a.md"]}}, {"id": "b", "independent": true, "task": {"files": ["issues/b.md"]}}]}}
```

CLI: `freellama task --task decision --questions @questions.json --input-file issue.md` prints the
JSON receipt; the answers are under `.response.answers`.

**Read the result:** from MCP and `fllama run`, one line per question, such as
`kind: bug (p 0.95, confidence 0.79)`.

**Check it:** spot-check 3 to 5 answers, and treat a low probability as unknown. Add an explicit `other` option when no listed label may fit. Do not use this model's `score` answers for severity.
