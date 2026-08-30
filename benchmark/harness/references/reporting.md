# Reporting Results

Load when producing or reading the dashboard. Why: one headline score can hide coverage and reliability failures.

Lead with deterministic pass rate and applicable coverage. Then show pass^3 reliability, category/tier scores, quality-gated successful tasks/hour, latency, tokens, cache use, tool failures, and memory.

The HTML dashboard must disclose suite version, runner version, trials, candidate and judge identities, missing capabilities, skipped/failed tasks, timestamps, and whether judge calibration was verified. Link each aggregate row to its raw trial file path.

Do not rank an MCP-capable system against a filesystem-only system on different denominators. Use common-task views for head-to-head claims. Treat one-trial results as smoke tests and public tasks as orientation.

Use Pareto language when quality and speed disagree: name the quality leader, efficiency leader, and dominated candidates rather than forcing a winner. A faster wrong answer never wins.

The report is rebuildable: edit neither aggregate JSON nor HTML by hand; regenerate them from raw trials.

This step ends after `scripts/validate.py` accepts the raw and aggregate artifacts.

