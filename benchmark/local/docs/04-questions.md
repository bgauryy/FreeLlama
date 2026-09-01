# The 30 questions

Three pinned repos, 10 read-only research/tracing questions each, basic -> advanced -> complex.
"Do not edit files" on every question — no bug injection, no fix-verification. This keeps grading
symmetric between an octocode-tool agent and a bash-only agent: both are judged purely on whether
they find and correctly explain real facts about the codebase, not on which tool vocabulary they
used to get there (see `05-grading-and-judge.md`).

**Question files contain only the prompt** — no answer keys, no grading hints, nothing that could
leak into an agent's context or bias a reader. The answer key and deterministic checks live solely
in `tasks/octocode-vs-bash-30.json` (the machine-graded source of truth), verified directly against
each pinned revision before being written down.

## click — [pallets/click](https://github.com/pallets/click) @ `2c8cd3a`

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

## zustand — [pmndrs/zustand](https://github.com/pmndrs/zustand) @ `b57db4f`

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

## openui — [thesysdev/openui](https://github.com/thesysdev/openui) @ `78913a1`

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

## Design notes

- Tiers deliberately widen scope, not only vocabulary difficulty: basic/Q1-Q2 are single-file
  lookups, core/Q3-Q6 require reading one function/class closely, advanced/Q7-Q9 require
  cross-referencing 2+ locations or a multi-step call chain, complex/Q10 requires connecting a field
  declaration, its initializer, and multiple methods across a large class or module.
- No task requires editing, running tests, or fixing a bug — both adapters expose only read/search
  tools (see `02-agent-a-octocode.md` / `03-agent-b-bash.md`), so `no_changes` should hold for every
  well-behaved trial regardless of agent.
- Every fact behind every question was verified directly against the pinned revision (`grep`/`sed -n`
  against the real source in `.context/`) before being written into the suite JSON — not generated
  or assumed.
