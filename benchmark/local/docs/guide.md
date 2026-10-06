# Local benchmark guide

The local benchmark compares bounded research adapters that FreeLlama also exposes through
`delegate_research`. Keep the model, corpus, runtime controls, and grading fixed while comparing
tool surfaces. Start with the [local benchmark README](../README.md) for prerequisites and commands,
and use the [shared adapter contract](07-adapter-contracts.md) for loop behavior, runtime defaults,
and measured caveats.

## Run flow

1. `prepare_repo.sh` clones and pins `click`, `zustand`, and `openui` into `.context/`. The operator
   prepares this corpus; the research models receive copied files rather than clone access.
2. `restart_ollama.sh` restarts Ollama and prepares port 11435, reusing a running FreeLlama `serve`
   or starting a passthrough `proxy`. This changes local service state; inspect the script and
   coordinate with other work before running it.
3. `run_all.sh` generates the model-specific matrix and invokes the generic `run_matrix.py`.
   The two adapters run sequentially, one question at a time, with a disposable workspace per
   task/trial and matched runtime settings.
4. `run.py` records each result and applies deterministic checks for required facts, evidence paths,
   and workspace changes. It writes one `trial-N.json` as soon as each question is graded and prints
   a `{"task":...,"status":...,"score":...}` progress line. `run_all.sh` discards workspace copies by
   default while retaining trial artifacts.
5. The matrix runner aggregates trials and renders the HTML dashboard. `run_all.sh` appends the
   invocation and results paths to the generated local `runs/index.jsonl` ledger.
6. An optional non-local post-hoc judge reviews completed answers separately. It does not run
   through `run_all.sh` or determine the deterministic pass rate. See [Grading](#grading).

The generic [harness](../../harness/README.md) owns execution, grading, aggregation, and reports.
The local directory owns adapters, the pinned corpus, and the comparison suite.

### Benchmark and MCP transport

| Invocation | Model transport | Management boundary |
|---|---|---|
| Local benchmark matrix | `FREELLAMA_OLLAMA_ENDPOINT`, normally the prepared serve/proxy at port 11435 | Raw `/api/chat` compatibility path; not a managed task |
| MCP `delegate_research` | `FREELLAMA_AGENT_MANAGED_ENDPOINT` | Every model turn is a managed coding task with admission and placement receipts |

Both adapters read `FREELLAMA_TARGET_MODEL`, falling back to `FREELLAMA_BENCH_MODEL`. The generated
matrix supplies the target Ollama tag separately from its unique benchmark entry ID. Workspace,
prompt, and result paths use the [generic adapter contract](../../harness/references/adapters.md).

Proxy retries and adapter retries cover distinct failures. The proxy retries transient upstream
failures (HTTP 500/502/504 or connection errors, up to 3 attempts; never 503 busy), so retries are
transparent and identical for both adapters. Read
[`transport.ts`](../../../packages/mcp/src/research/transport.ts) and the
[shared adapter contract](07-adapter-contracts.md) before interpreting infrastructure errors as
model failures or changing retry settings.

### Change the comparison

| Change | Action |
|---|---|
| Installed model | Pass one exact tag to `./scripts/run_all.sh --model <tag>`; this does not pull it |
| Reliability trials | Pass `--trials 3`; one trial is a smoke result |
| Runtime budget | Set matched `FREELLAMA_AGENT_*` overrides for both adapters and record them |
| Questions | Update the suite and prompt-only question files before freezing a comparison |
| Corpus revision | Change `prepare_repo.sh`, then re-verify answer keys against every pinned source revision |
| Adapter behavior | The prompts were tuned against this corpus; disclose that and evaluate on repositories outside `.context/` before claiming the change generalizes |

Keep suites, fixtures, answer keys, graders, and schemas frozen during a comparison. Preserve raw
trial JSON; aggregate JSON and HTML are rebuilt views. Results from different model, transport,
context-policy, or adapter revisions require those differences to be disclosed.

## Adapters

Both adapters expose read-only research tools. The model emits one JSON action per turn; full tool
output is retained in the audit trail and served to the model in pages, a `page` action recovers
stored evidence without rerunning a tool, exact repeats return the prior step, and invalid JSON
receives a bounded repair notice. The caller retains task decomposition, judgment, mutation
authority, and final verification.

### Octocode adapter

The Octocode adapter lets a local Ollama model look up repository facts through five structured
code-navigation tools. FreeLlama's `delegate_research {adapter:"octocode"}` uses it. The
implementation lives in [`octocode-agent.ts`](../../../packages/mcp/src/research/octocode-agent.ts) and the exact generated prompt in
[`prompts.ts`](../../../packages/mcp/src/research/prompts.ts);
the [MCP reference](../../../packages/mcp/README.md) covers caller inputs and verification verdicts.

| Tool | Purpose | Caveat |
|---|---|---|
| `localViewStructure` | Inspect a directory or file structure | Scope the path before reading content |
| `localFindFiles` | Find file names or paths | An empty filtered search does not prove absence |
| `localSearchCode` | Find text or regular-expression matches | Structural mode requires `pattern` or `rule`; `keywords` alone is invalid |
| `localGetFileContent` | Read exact file content or a bounded range | Use real search anchors and retain source evidence |
| `lspGetSemantics` | Resolve definitions, references, or symbol outlines | Empty results can reflect indexing; retry or use text search |

Actions are `octocode`, `page`, or `finish`. `parse_action` checks the supported tool and required
action fields; the Octocode CLI validates each tool's query contract. `safe_resolve` resolves `path`
and `uri` against the workspace, rejects escapes through symlinks, and excludes Git metadata.

The adapter invokes `npx --yes` with the pinned `octocode@19.1.0` package, the tool name, serialized
queries, and compact output. `FREELLAMA_AGENT_OCTOCODE_PACKAGE` overrides that package deliberately.
A globally installed CLI is not required. Prepare the pinned package before a timed comparison so
first-use package download does not consume a tool deadline or distort the trial.

### Bash adapter

The Bash adapter is the default adapter for `delegate_research` and the baseline for the Octocode
comparison. The adapter lives in [`bash-agent.ts`](../../../packages/mcp/src/research/bash-agent.ts), its prompt in
[`prompts.ts`](../../../packages/mcp/src/research/prompts.ts), and the shared task loop in [`agent-loop.ts`](../../../packages/mcp/src/research/agent-loop.ts);
confinement lives in [`sandbox.ts`](../../../packages/mcp/src/research/sandbox.ts). Actions are `shell`, `page`,
or `finish`; a shell action contains one command string that the sandbox validates before executing
it in the workspace.

| Control | Behavior |
|---|---|
| Tool allowlist | Read-only utilities such as `rg`, `grep`, `cat`, `find`, `jq`, bounded `sed`, and read-only Git subcommands |
| Composition | Pipes, `&&`, and `;` join validated commands |
| Writing and execution flags | Refuses output redirection, `find -exec/-delete`, `sort -o`, `rg --pre`, and Git configuration/execution overrides |
| Hidden execution | Refuses command substitution, backticks, variable expansion, loops, and subshells |
| Path confinement | Refuses paths or symlinks outside the workspace and recursive symlink-following options |
| Indirect file access | Refuses script files and filesystem-reading `xargs` targets; output-only targets are allowed |
| General executors | Does not allow `awk`, Python, or network tools |

Execution uses restricted Bash, a path containing only allowlisted tools, and a scrubbed
environment. An OS sandbox adds confinement when available: `bwrap` on Linux or `sandbox-exec` on
macOS. `FREELLAMA_AGENT_OS_SANDBOX=off` disables that OS layer while keeping command validation and
the restricted shell. Results record the selected sandbox in `model_metadata.sandbox`. These controls
apply even when MCP delegation reads the caller's real workspace; a disposable benchmark copy is not
the safety boundary. A refused command consumes a turn.

### Compare the adapters

Use the same exact model, decoding controls, context/turn budgets, fixture revisions, transport,
and grading rules. Record tool timeouts and package/index warm-up because the tool surfaces have
different overhead. Measure correctness, tokens, calls, and time separately; passing faster does
not establish better answers, and a structured tool does not imply fewer calls. Run the confinement
and action contracts through `yarn test:agents` from the repository root.

## Questions

Three pinned repositories, 10 read-only research/tracing questions each, basic to complex. Every
question says "Do not edit files", so grading stays symmetric between the two tool surfaces: both
are judged on whether they find and correctly explain real facts, not on tool vocabulary.

Question files contain only the prompt. The answer key and deterministic checks live solely in
`tasks/octocode-vs-bash-30.json`, verified against each pinned revision (`grep`/`sed -n` against the
real source in `.context/`) before being written down.

Tiers widen scope: basic (Q1-Q2) are single-file lookups, core (Q3-Q6) require reading one
function or class closely, advanced (Q7-Q9) cross-reference 2+ locations or a call chain, and
complex (Q10) connects a declaration, its initializer, and several methods across a large module.
No task requires editing or running tests, so `no_changes` should hold for every well-behaved trial.

### click — [pallets/click](https://github.com/pallets/click) @ `2c8cd3a`

| Q | Tier | Category | File |
|---|------|----------|------|
| L01 | basic | task_understanding | [questions/click/Q1.md](questions/click/Q1.md) |
| L02 | basic | tool_use | [questions/click/Q2.md](questions/click/Q2.md) |
| L03 | core | reasoning | [questions/click/Q3.md](questions/click/Q3.md) |
| L04 | core | task_understanding | [questions/click/Q4.md](questions/click/Q4.md) |
| L05 | core | reasoning | [questions/click/Q5.md](questions/click/Q5.md) |
| L06 | core | reasoning | [questions/click/Q6.md](questions/click/Q6.md) |
| L07 | advanced | efficiency | [questions/click/Q7.md](questions/click/Q7.md) |
| L08 | advanced | reasoning | [questions/click/Q8.md](questions/click/Q8.md) |
| L09 | advanced | end_to_end | [questions/click/Q9.md](questions/click/Q9.md) |
| L10 | complex | reasoning | [questions/click/Q10.md](questions/click/Q10.md) |

### zustand — [pmndrs/zustand](https://github.com/pmndrs/zustand) @ `b57db4f`

| Q | Tier | Category | File |
|---|------|----------|------|
| Z01 | basic | task_understanding | [questions/zustand/Q1.md](questions/zustand/Q1.md) |
| Z02 | basic | tool_use | [questions/zustand/Q2.md](questions/zustand/Q2.md) |
| Z03 | core | reasoning | [questions/zustand/Q3.md](questions/zustand/Q3.md) |
| Z04 | core | task_understanding | [questions/zustand/Q4.md](questions/zustand/Q4.md) |
| Z05 | core | reasoning | [questions/zustand/Q5.md](questions/zustand/Q5.md) |
| Z06 | core | reasoning | [questions/zustand/Q6.md](questions/zustand/Q6.md) |
| Z07 | advanced | efficiency | [questions/zustand/Q7.md](questions/zustand/Q7.md) |
| Z08 | advanced | reasoning | [questions/zustand/Q8.md](questions/zustand/Q8.md) |
| Z09 | advanced | end_to_end | [questions/zustand/Q9.md](questions/zustand/Q9.md) |
| Z10 | complex | reasoning | [questions/zustand/Q10.md](questions/zustand/Q10.md) |

### openui — [thesysdev/openui](https://github.com/thesysdev/openui) @ `78913a1`

| Q | Tier | Category | File |
|---|------|----------|------|
| O01 | basic | task_understanding | [questions/openui/Q1.md](questions/openui/Q1.md) |
| O02 | basic | tool_use | [questions/openui/Q2.md](questions/openui/Q2.md) |
| O03 | core | task_understanding | [questions/openui/Q3.md](questions/openui/Q3.md) |
| O04 | core | reasoning | [questions/openui/Q4.md](questions/openui/Q4.md) |
| O05 | core | reasoning | [questions/openui/Q5.md](questions/openui/Q5.md) |
| O06 | advanced | reasoning | [questions/openui/Q6.md](questions/openui/Q6.md) |
| O07 | advanced | reasoning | [questions/openui/Q7.md](questions/openui/Q7.md) |
| O08 | advanced | efficiency | [questions/openui/Q8.md](questions/openui/Q8.md) |
| O09 | advanced | end_to_end | [questions/openui/Q9.md](questions/openui/Q9.md) |
| O10 | complex | reasoning | [questions/openui/Q10.md](questions/openui/Q10.md) |

## Grading

The benchmark has two separate measurement passes; `run_all.sh` performs only the first.

1. **While the local models run:** deterministic checks and resource metrics only. Every task uses
   the same three tool-name-agnostic checks (`response_contains`, `evidence_paths_exist`,
   `no_changes`). They are the authoritative anchor: an unambiguous pass/fail, not a judgment call.
   `aggregate.py` adds a mechanical 0-10 per-task efficiency score relative to whichever adapter used
   fewer tokens, tool calls, and wall time to reach a deterministically passing answer.
2. **Optional post-hoc review:** after both adapters finish all 30 questions, an independent,
   non-local model can judge each answer. A non-local orchestrator (never a local model) performs
   this step manually; it is not part of the deterministic score.

### Why no local judge

An early run scored every trial with a local judge (`qwen2.5:32b`) while the ~30GB candidate model
stayed resident: ~58GB against a 48GB unified-memory budget. Ollama returned intermittent HTTP 500
errors that corrupted about a third of the trials through infrastructure failure, not adapter
behavior. The matrix therefore sets `"judge_model": null`, and judging runs later with a model that
was never resident on the same machine. A later run without the judge still saw transient HTTP 500
errors on ~8% of trials, mostly on the longer Octocode conversations; that motivated the proxy
retries described in [Benchmark and MCP transport](#benchmark-and-mcp-transport).

### Post-hoc judge design

- **Reference-grounded.** Score each answer against the verified answer key, not head-to-head.
- **Blind to identity and order.** Present answers without the adapter, tool, or model that
  produced them, and score each independently to avoid position bias.
- **Independent rubric scores.** Score 0-5 for correctness, evidence, and completeness per question,
  scaled to 0-100; derive the comparison from the two independent scores.
- **Different-family judge.** Use a judge from a different model family than the candidate.
- **Fresh context per question.** The judge has no memory of writing the question, its answer key,
  or other questions' grading.

Sources: [Mitigating the Bias of Large Language Model Evaluation](https://arxiv.org/pdf/2409.16788);
[Judging the Judges](https://arxiv.org/pdf/2604.23178).

## Results

This section is a map, not a scorecard; do not paste pass rates into the repository.

| Where | What |
|---|---|
| `results/<model-slug>/index.html` | Dashboard for that model (gitignored; produced by `run_all.sh`) |
| `results/<model-slug>/aggregate.json` | Rebuildable aggregate (gitignored) |
| `runs/index.jsonl` | Generated local ledger, one line per `run_all.sh` invocation; commit it intentionally for a durable shared index |
| [`task-delegation.md`](../../../skills/freellama/references/task-delegation.md) | Measured Octocode-vs-Bash notes used by the product |

Point at a run ID in `runs/index.jsonl` and preserve the raw trial artifacts that produced it.
