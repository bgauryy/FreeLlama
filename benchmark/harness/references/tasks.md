# Task Suite

Load when selecting cases or interpreting skipped tasks. Why: capability groups must not be mixed silently.

The frozen suite contains five tasks per tier:

| Tier | Tasks | Focus |
|---|---|---|
| basic | Q01–Q05 | structured output, explanation, tool choice, small fix, orientation |
| core | Q06–Q10 | large-repo search, impact, feature work, restraint, diagnosis |
| advanced | Q11–Q15 | multi-file repair, regressions, recovery, efficiency, MCP |
| complex | Q16–Q20 | skills, conflicts, refactor, performance, end-to-end work |

Requirements are explicit: `filesystem`, `shell`, `tools`, `skills`, or `mcp`. A missing requirement yields `not_applicable`; it is excluded from denominators and shown in coverage.

Each trial starts from a fresh copy of `fixtures/atlas`. Task checks inspect the final answer, normalized tool trace, changed files, or an external verifier. Public prompts orient; `scripts/build_private_suite.py` creates unpredictable fixture facts and expected values outside the skill folder for promotion gates.

Do not edit prompts or checks after seeing a candidate result. Version the suite instead.

Next: load `methodology.md` before comparing scores.
