# Reporting offloaded work

Load when handing the result of FreeLlama work to the user or the next step.

## Use the local answer

- Pass on the local model's answer only after its `check:` lines are handled and 3–5 items (or the single answer) were checked against the source. Fix or drop what failed; never forward a raw local answer as fact.
- A code edit from a workflow is a candidate: apply a SEARCH/REPLACE block only where its SEARCH text matches exactly once, then run the tests.
- Keep large outputs in files (`output_file`, `--save-as`, `response_file`) and cite the path instead of pasting them.
- Where results live: FreeLlama writes `saveAs` handoffs to `$FREELLAMA_HOME/handoffs/` (default `~/.octocode/freellama/handoffs/`). Save a report of your own under `<workspace>/.octocode/freellama/` (scratch under `<workspace>/.octocode/freellama/tmp/`), and only when the user wants one kept.

## Report

| Field | Say |
|---|---|
| Result | What came back, in your words, with the verified parts marked as verified |
| Model | The tag that ran (receipt `[model]`, `--verbose`, `job_profile.model`), and the workflow or job |
| Checks | Each `check:` line or `[unsure]` mark and what you did about it |
| Verified | What you checked and how (items spot-checked, tests run); what you did not check |
| Refusals | For exit 75 or a memory refusal: free memory, what holds it, and the retry or smaller pick you chose. Never stop the user's processes |
| Approvals | Any pull, unload or delete, with the exact tag and the user's approval; otherwise say none happened |

Keep it short: one line per field that applies. An `escalate` result is reported as discarded, with the work you did instead.
