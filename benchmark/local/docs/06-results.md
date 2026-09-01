# Results

This file is a map, not a scorecard. Numbers go stale the moment a new `run_all.sh` finishes.

| Where | What |
|---|---|
| `results/<model-slug>/index.html` | dashboard for that model (gitignored; produced by `run_all.sh`) |
| `results/<model-slug>/aggregate.json` | rebuildable aggregate (gitignored) |
| `runs/index.jsonl` | generated local ledger: one line per `run_all.sh` invocation; commit it intentionally if you need a durable shared index |
| [`skills/freellama/references/task-delegation.md`](../../../skills/freellama/references/task-delegation.md) | measured octocode-vs-bash notes used by the product |

Do not paste pass rates into this file. Point at a run ID in `runs/index.jsonl` and preserve the raw
trial artifacts that produced it.
