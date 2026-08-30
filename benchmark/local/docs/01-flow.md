# Flow

```
 1. prepare_repo.sh   RUNNER-only: clone+pin click, zustand, openui into .context/ (gitignored)
 2. restart_ollama.sh kill + relaunch Ollama, then build+start the FreeLlama proxy in front of
                        it (127.0.0.1:11435 -> 127.0.0.1:11434). Both agents talk to the proxy, not
                        raw Ollama — it adds retry-with-backoff on transient 5xx errors
                        (packages/rust-core/src/proxy.rs), which fixed a real ~8% infra-flakiness rate (see
                        docs/05-grading-and-judge.md)
 3. run_matrix.py      runs the 2 agents SEQUENTIALLY (never in parallel), each fully working
                        through all 30 questions one at a time before the next agent starts:
                          - copy .context/ into a fresh disposable workspace (agents never clone)
                          - write the question to prompt.md
                          - spawn the agent adapter as a subprocess (own process group, timeout,
                            RSS sampling)
                          - adapter drives its own chat loop against Ollama's /api/chat
                          - adapter writes agent-result.json (answer, tool_calls, token usage)
                          - run.py deterministically grades the answer (response_contains,
                            evidence_paths_exist, no_changes) — no LLM judge runs during this step
                          - one trial-N.json is written and one status line is printed THE MOMENT
                            each question finishes — results are durable per-question, not batched
 4. aggregate.py       scans every trial-*.json, computes per-question and per-agent medians
                        (tokens, tool calls, wall time) and pass rates
 5. judge pass          orchestrator-only, non-local (see docs/05-grading-and-judge.md): once BOTH
                        agents have finished all 30 questions, Claude/Codex independently scores
                        every answer against the verified reference, blind to which agent produced
                        it — never a local model, never run.py/distilled_judge.py
 6. render_html.py     turns aggregate.json (+ judge scores) into results/index.html
```

`run_all.sh` runs steps 3-4. Steps 1-2 are separate because you normally only need to redo them once
per machine / once per repo-revision change. Step 5 is deliberately NOT part of `run_all.sh` — it is
performed by the orchestrator after inspecting that both agents finished cleanly, per
`docs/05-grading-and-judge.md`.

## Why an existing harness, not a new one

`benchmark/harness/scripts/{run.py,run_matrix.py,aggregate.py,render_html.py,distilled_judge.py}`
are suite-and-agent-agnostic: they consume a suite JSON (questions + grading checks), a matrix JSON
(which agent-command to run per model id), and any adapter that reads four env vars and writes one
JSON result file. Nothing in this benchmark modifies those scripts — it only adds a new suite, a new
matrix, and two new adapters. See `benchmark/harness/references/adapters.md` for the exact
adapter contract both `octocode_agent.py` and `bash_agent.py` implement:

- Read `FREELLAMA_BENCH_MODEL`, `FREELLAMA_BENCH_PROMPT` (path to the question text),
  `FREELLAMA_BENCH_WORKSPACE` (path to the disposable repo copy), `FREELLAMA_AGENT_RESULT`
  (path to write the result JSON to). Both matrix entries also set `FREELLAMA_TARGET_MODEL` (via
  `env FREELLAMA_TARGET_MODEL=... python3 ...` in `agent_command`, filled in by `run_all.sh --model`)
  — the actual Ollama model name to call, since `FREELLAMA_BENCH_MODEL` is fixed to the matrix
  entry's `id` (`<model-slug>-octocode` / `<model-slug>-bash`), which must stay unique per entry so
  results don't collide, but isn't itself a valid `ollama` tag.
- Drive a chat loop against `FREELLAMA_OLLAMA_ENDPOINT` (default `http://127.0.0.1:11434`, but
  `run_all.sh` sets it to the FreeLlama proxy at `http://127.0.0.1:11435` — see above), using
  `/api/chat`, `format:"json"`, temperature 0, seed 42, `num_ctx 8192`, `num_predict 512`, max 10
  turns — identical decoding settings for both agents, so the only variable being measured is the
  tool surface.
- Normalize every tool invocation into `tool_calls[]` (`name`, `arguments`, `status`, `duration_ms`,
  `result`) and token counts into `usage{input_tokens,output_tokens}`.
- Exit 0 on success, 1 on failure; never edit files outside the workspace copy.

## Changing things later

- **Different model:** `./scripts/run_all.sh --model <ollama-tag>` — one flag, no file edits.
  `run_all.sh` fills `tasks/octocode-vs-bash-matrix.template.json`'s `__MODEL__`/`__MODEL_SLUG__`
  placeholders and writes the concrete matrix to `tasks/.generated/matrix-<slug>.json` (gitignored,
  regenerated every run). Each model's results land in their own `results/<slug>/`, and the run gets
  logged to `runs/index.jsonl` (run_id, model, date, pass rate, results path) — so testing several
  models never overwrites a prior model's data or loses track of which run_id used which model.
- **More trials:** `./scripts/run_all.sh --trials 3` (default 1; the existing real-repos-10 suite
  uses 3 for publishable reliability — see `benchmark/harness/references/methodology.md`).
- **Different/more questions:** edit `tasks/octocode-vs-bash-30.json` and add a matching
  `docs/questions/<repo>/QN.md`; keep `checks[]` tool-name-agnostic (no `tool_required_any`/
  `tool_forbidden`) so the two agents are graded on outcome, not on which tool family they used —
  see `docs/05-grading-and-judge.md` for why. Question files hold only the prompt (no answer keys,
  no grading hints) — the answer key lives solely in the suite JSON's `response_contains` values.
- **Different/more target repos:** add a `clone_pinned` call to `scripts/prepare_repo.sh` (clone URL
  + pinned SHA) — every fact in the suite was verified against the exact pinned revisions by reading
  the real source, so a repo/revision change invalidates the answer keys until re-verified.
