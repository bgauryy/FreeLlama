# Local benchmark run flow

The local benchmark compares bounded research adapters that FreeLlama also exposes through
`delegate_research`. Keep the model, corpus, runtime controls, and grading fixed while comparing
tool surfaces. Start with the [local benchmark guide](../README.md) for prerequisites and commands.

## Prepare and run

1. `prepare_repo.sh` clones and pins `click`, `zustand`, and `openui` into `.context/`. The operator
   prepares this corpus; the research models receive copied files rather than clone access.
2. `restart_ollama.sh` restarts Ollama and prepares port 11435, reusing a running FreeLlama `serve`
   or starting a passthrough `proxy`. This changes local service state; inspect the script and
   coordinate with other work before running it.
3. `run_all.sh` generates the model-specific matrix and invokes the generic `run_matrix.py`.
   The two adapters run sequentially, one question at a time, with a disposable workspace per
   task/trial and matched runtime settings.
4. `run.py` records each result and applies deterministic checks for required facts, evidence paths,
   and workspace changes. Completed question receipts are written individually. `run_all.sh`
   discards workspace copies by default while retaining trial artifacts.
5. The matrix runner aggregates trials and renders the HTML dashboard. `run_all.sh` appends the
   invocation and results paths to the generated local `runs/index.jsonl` ledger.
6. An optional non-local post-hoc judge reviews completed answers separately. It does not run
   automatically through `run_all.sh` or determine the deterministic pass rate. Use the
   [grading guide](05-grading-and-judge.md) for this separate review.

The generic [harness](../../harness/README.md) owns execution, grading, aggregation, and reports.
The local directory owns adapters, the pinned corpus, and the comparison suite.

## Distinguish benchmark and MCP transport

| Invocation | Model transport | Management boundary |
|---|---|---|
| Local benchmark matrix | `FREELLAMA_OLLAMA_ENDPOINT`, normally the prepared serve/proxy at port 11435 | Raw `/api/chat` compatibility path; not a managed task |
| MCP `delegate_research` | `FREELLAMA_AGENT_MANAGED_ENDPOINT` | Every model turn is a managed coding task with admission and placement receipts |

Both adapters read `FREELLAMA_TARGET_MODEL`, falling back to `FREELLAMA_BENCH_MODEL`. The generated
matrix supplies the target Ollama tag separately from its unique benchmark entry ID. Workspace,
prompt, and result paths use the [generic adapter contract](../../harness/references/adapters.md).

Proxy retries and adapter retries cover distinct failures. Read
[`agent_transport.py`](../scripts/agent_transport.py) and the
[shared adapter contract](07-adapter-contracts.md) before interpreting infrastructure errors as
model failures or changing retry settings.

## Change the comparison

| Change | Action |
|---|---|
| Installed model | Pass one exact tag to `./scripts/run_all.sh --model <tag>`; this does not pull it |
| Reliability trials | Pass `--trials 3`; one trial is a smoke result |
| Runtime budget | Set matched `FREELLAMA_AGENT_*` overrides for both adapters and record them |
| Questions | Update the suite and prompt-only question files before freezing a comparison |
| Corpus revision | Change `prepare_repo.sh`, then re-verify answer keys against every pinned source revision |
| Adapter behavior | Use the [held-out evaluation](../../holdout/README.md) for acceptance evidence |

Keep suites, fixtures, answer keys, graders, and schemas frozen during a comparison. Preserve raw
trial JSON; aggregate JSON and HTML are rebuilt views. Results from different model, transport,
context-policy, or adapter revisions require those differences to be disclosed.
