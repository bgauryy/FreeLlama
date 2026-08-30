# Distilled Judging

Load when enabling a local LLM judge. Why: qualitative scores need calibration and bias controls.

The judge receives the prompt, rubric, deterministic check results, final answer, changed-file summary, and sanitized tool trace. It does not receive candidate identity, speed, token use, or leaderboard position.

Return JSON scores from 0–5 for `correctness`, `evidence`, `coherence`, and `structure`, plus short evidence-backed comments and `confidence`. Allow `unknown`; never invent missing evidence.

Calibration input is `{"cases":[...]}`; every case contains `human_winner`, `judge_winner`, `swapped_judge_winner` (`A|B|TIE`) and integer `human_score`/`judge_score` (`0..5`).

Use a different model family from the candidate when possible. Build a dated artifact with `scripts/calibrate_judge.py`; its gates are ≥20 labels, ≥85% pairwise agreement, weighted κ≥0.70, and ≥95% order-swap consistency. Calibration expires after 30 days by default.

Close or contested comparisons require answer-order swapping or a second judge. Deterministic test results override a contradictory qualitative judgment. Do not grade hidden chain-of-thought; grade observable decisions, evidence, artifacts, and recovery.

`scripts/distilled_judge.py` is an Ollama adapter. Pass its matching, unexpired calibration to `run.py --judge-calibration`; otherwise scores stay advisory.

Next: aggregate with the AGGREGATE route in `SKILL.md`.
