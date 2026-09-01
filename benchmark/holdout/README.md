# Held-out delegation eval

The rest of `benchmark/` measures **models**. This measures the **adapter** — the agent loop that
`delegate_research` runs — against repositories it was never tuned on.

## Why this exists separately

FreeLlama's own repository is contaminated as an eval corpus. The adapter prompts (search scoping,
declaration-vs-occurrence guidance, the JSON repair loop) were all tuned against questions about
this repository, across repeated runs. Numbers measured there are training-set performance and cannot
support an accept/reject decision about the adapter.

`benchmark/local/` has the same problem in a milder form: its `click` / `zustand` / `openui` corpora
are checked into this project's history and have been run many times.

So this harness pulls **fresh upstream repositories** into `.clones/` (gitignored, never part of
this repository) and grades against ground truth derived from their AST.

## Two conditions, because the first experiment could not discriminate

Round one scored an identical **12/15 on both arms** and looked like a null result. It was a harness
fault. The change under test scopes searches away from build and vendor directories — and a
`--depth 1` clone has none, so there was nothing for it to fix. A held-out set that lacks the
condition a fix targets cannot measure that fix in either direction.

`make_dirty.py` builds the representative condition: a working directory, not a checkout. The
important part is a **vendored copy of the package inside its own virtualenv**
(`.venv/lib/python3.13/site-packages/<pkg>/`), plus a `build/lib/` staging copy, stale `__pycache__`
bytecode, and `node_modules`/`dist` decoys that mention the same identifiers.

```
grep -rn "def build_digest_header" .clones/requests-dirty   ->  5 hits, 3 of them real copies
                             ... with scoping excludes      ->  1
```

Measure that from the *agent's* shell, not yours. This machine's interactive `grep` is `ugrep`,
which smart-skips hidden directories and reports 1 hit; the adapter's `subprocess` gets
`/usr/bin/grep` (BSD) and sees all 5. Judging difficulty from the wrong shell understates it.

## Question tiers

Balanced capability-versus-regression, following repository-level QA benchmark design where the
discriminating signal is cross-file, multi-hop reasoning — dependency tracing, feature localization,
intent inference (SWE-QA arXiv:2509.14635, DeepRepoQA arXiv:2608.24221, CoReQA arXiv:2501.03447).

| Tier | Kinds | Expectation |
|---|---|---|
| `regression` | `location`, `constant` | near-100%; must not break |
| `capability` | `complexity`, `signature`, `decorator`, `inheritance` | single file, real work |
| `advanced` | `callsite`, `import_origin`, `raises` | cross-file / multi-hop |

Note what the dirty condition does to the *regression* tier: "which file defines `X`" has one
correct answer and three files containing it. Regression questions become discriminating when the
workspace contains realistic build and vendor copies.

## Accept-set grading

Each case ships several accepted forms. Strict single-form matching is the largest source of false
negatives in answer grading, and it punishes an answer for being *more* informative — full path
instead of basename, `Class.method` instead of `method`. No accepted form can be produced by an
agent that found nothing, so the grader stays honest without being brittle. The grader normalizes
answers for case, backticks, quotes, and trailing parentheses first.

Every miss carries a `failureSignature` — `no_answer`, `unparseable_reply`,
`ungrounded_no_tool_calls`, `all_tool_calls_failed`, `turn_budget_exhausted`, `wrong_answer` — so a
zero is interpretable instead of opaque.

## The anchor: ground truth no one can argue with

`build_truth.py` computes every expected answer with Python's `ast` module, before any model runs:

| Kind | Question | Truth source |
|---|---|---|
| `complexity` | which function in this file has the most branch statements (`if`/`for`/`while`/`try`/`except`/`with`) | AST walk, per function |
| `location` | which file defines this symbol | AST, symbols defined exactly once in the package |
| `constant` | value of a module-level constant | AST literal |

Cases are dropped rather than graded loosely. A complexity case needs a **strict** winner (no tie
with the runner-up, at least 5 branches). A location case needs a symbol defined **exactly once**.

**A grader that can pass by accident is not a grader.** Two earlier iterations of this file produced
expected values like `10`, `{{`, `%}`, `__init__`, `parse`, and `alias`—all of which appear in
ordinary prose, so a run that found nothing can still score a hit. The filters require
distinctive identifiers (≥6 characters, no dunders, not in `GENERIC_NAMES`) and constants that are
not bare words. Selection is a fixed stride over a sorted pool, so it is reproducible and not
cherry-picked.

## Run the evaluation

```bash
mkdir -p .clones && cd .clones
git clone --depth 1 https://github.com/psf/requests requests
git clone --depth 1 https://github.com/pallets/jinja jinja
cd -

python3 benchmark/holdout/build_truth.py                        # -> .clones/_eval/truth.json
python3 benchmark/holdout/make_dirty.py                         # -> .clones/*-dirty
python3 benchmark/holdout/run_holdout.py --condition dirty --limit 24
python3 benchmark/holdout/run_holdout.py --condition clean      # control
```

`--trials K` repeats the suite: a single green is not a result (pass@1 vs pass^k). Point
`HOLDOUT_BASELINE` at the adapter you are comparing against.

`run_holdout.py` needs `freellama serve` up and a capable local model installed. It runs two arms
over the same frozen cases:

- **baseline** — the adapter as it stood before the changes under test
- **current** — `benchmark/local/scripts/bash_agent.py` as it is now

Point `BASELINE_ADAPTER` at whatever "before" you are testing against; keep the case set frozen
across both arms.

## Decision rule

ACCEPT the adapter change only if held-out accuracy beats baseline **and** every guardrail holds:

| Guardrail | Threshold | Why it cannot be tuned by the thing under test |
|---|---|---|
| accept-precision | ≥ 90% | the architecture's trust model depends on `accept` meaning correct; an adapter that got more answers right by making the verdict less discriminating has not improved |
| token offload | ≥ 90% | accuracy bought by returning the whole file is not offload |
| wall clock | ≤ 120s/question | an adapter that wins by grinding through the turn budget has traded the wrong resource |

Primary metric improving while a guardrail degrades means the goal was framed wrong — reframe it,
do not ship the loop.

## Rules carried over from `benchmark/harness/`

- Freeze cases, graders and the harness during a comparison; evolve them only *between* experiments.
- Three trials for a publishable claim. One trial is a smoke result.
- Preserve raw JSON; aggregates are rebuildable views, not sources of truth.
