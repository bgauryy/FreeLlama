# Sweep a large job in small, checked chunks

**Use when** a check is too big to read yourself but splits cleanly: every file of a code tree, a long log, the pages of a docs site. A small local model judges each chunk with your questions, in the background and one chunk at a time. Every result is saved as it lands, and you read only what was flagged.

**Skip it when** there are only a few items (use `run_task` with `tasks`), or when a missed finding would be costly and you will not verify the flagged ones.

**Models:** a decision model for triage, such as the catalog's `classify` pick `strands-decider:2b-nvfp4`. Optionally add a stronger `verifyModel` that re-asks only what was flagged or unsure, and an `explainModel` with an `explainPrompt` that writes one note per confirmed flag. See `models {view:"catalog", job:"classify"}` for what is installed and fits now.

## How it works

1. **Split.** `files` (paths or globs) or `urls` (a polite crawl of public pages: robots.txt honored, GET only, at most 200 pages, no local or private hosts) are cut into labelled chunks of about 750 tokens: `path:Lstart-Lend` or `url#n`.
2. **Triage.** The decision model answers your `questions` for every chunk. Yes/no (`noul`) questions flag a chunk at probability 0.5 or more; `flags` sets other rules, such as `{question:"kind", when:"bug", atLeast:0.7}`. A probability just below a threshold is **unsure**, not clear.
3. **Verify and explain.** Flagged and unsure chunks are re-asked by `verifyModel`. Chunks that stay flagged get a short note from `explainModel`.
4. **Save.** Each result is appended to `~/.octocode/freellama/sweeps/<id>/results.jsonl` before the next chunk starts. Transient refusals retry; memory refusals wait for memory. After a cancel or a server restart, `resume` continues where the sweep stopped.
5. **Compare.** Pass `baseline` (an earlier sweep of the same source) and unchanged chunks reuse its results. The status then reports what is new, changed, unchanged and removed, so a re-run costs only what changed.

## Calls

Start a code check; it returns at once:

```json
{"tool": "sweep", "arguments": {"action": "start", "name": "error handling", "files": ["src/**/*.ts"], "model": "strands-decider:2b-nvfp4", "questions": {"swallowed": {"type": "noul", "instructions": "Does this code catch an error and silently ignore it?"}}, "verifyModel": "bonsai-2:9b"}}
```

Follow it. Each call blocks up to 50 s and streams progress when the client sends a progressToken:

```json
{"tool": "sweep", "arguments": {"action": "wait", "sweepId": "00000000-0000-4000-8000-000000000000"}}
```

Read the flagged chunks, then open the cited ranges yourself:

```json
{"tool": "sweep", "arguments": {"action": "results", "sweepId": "00000000-0000-4000-8000-000000000000", "filter": "flagged"}}
```

Hand every flagged chunk to another task: `saveAs` writes them, under a summary of the counts, to `handoffs/<name>.md` and returns `handoff_file`, which a later `run_task` reads with `files`:

```json
{"tool": "sweep", "arguments": {"action": "results", "sweepId": "00000000-0000-4000-8000-000000000000", "filter": "flagged", "saveAs": "swallowed-errors"}}
```

Check a docs site for outdated install steps, two link hops deep:

```json
{"tool": "sweep", "arguments": {"action": "start", "urls": ["https://docs.example.com/"], "crawl": {"maxPages": 40, "maxDepth": 2}, "model": "strands-decider:2b-nvfp4", "questions": {"outdated": {"type": "noul", "instructions": "Does this page tell the reader to run a command or install a version that looks deprecated?"}}}}
```

Re-run next week and see only what happened since:

```json
{"tool": "sweep", "arguments": {"action": "start", "files": ["src/**/*.ts"], "model": "strands-decider:2b-nvfp4", "questions": {"swallowed": {"type": "noul", "instructions": "Does this code catch an error and silently ignore it?"}}, "baseline": "00000000-0000-4000-8000-000000000000"}}
```

```json
{"tool": "sweep", "arguments": {"action": "results", "sweepId": "00000000-0000-4000-8000-000000000000", "filter": "changed"}}
```

## Split the work well

- One question per concern, phrased about one chunk ("Does this code…"), never about the whole repository.
- Put judgment you must stand behind in your own review of the flagged chunks, not in the questions.
- Keep one model per sweep. `maxParallelism` stays 1 unless the machine has spare slots (`doctor {view:"status"}`).
- Split a source of more than 5,000 chunks into several sweeps, for example one per directory.

**Check it:** read 3 to 5 flagged chunks and 2 clear ones before trusting the rest. Raise a threshold with `atLeast` when too much is flagged, and add `verifyModel` when too little of what is flagged holds up.
