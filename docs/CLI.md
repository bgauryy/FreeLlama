# CLI reference

Moved out of the README so the front page can stay at "what / why / run it". Every subcommand,
policy generation, and the objectives that need a policy file live here.


Start the control plane, then drive it from another terminal:

```bash
npx freellama serve --recommendation-catalog recommendations.example.toml
```

```bash
npx freellama tools                                  # every MCP tool and its CLI equivalent
npx freellama models                                 # installed models, capabilities, residency
npx freellama route --task coding --objective fastest
npx freellama task --task completion --objective fastest "Reply with exactly OK."
npx freellama doctor                                 # works without serve
```

`doctor` is the only subcommand that runs without `serve`. Every other control-plane command now
tells you exactly how to start it if it is not running, rather than surfacing a transport error.

**Objectives.** `fastest` needs no configuration. `balanced` and `quality` require a policy file —
see "Trustworthy routing" below.

### Make routing trustworthy

By default every route grades `confidence: "low"`, because nothing has vouched for any model. To
get `medium` — and to make `minConfidence: "medium"` useful rather than a switch that refuses
everything — the router needs **two** inputs:

| Input | Supplies |
|---|---|
| a policy file | a *quality* contract: which models are vouched for on this task |
| a benchmark report | local *functional* measurement, from `npx freellama bench-all` |

Generate the policy from quality data — never from `bench-all`, which measures throughput:

```bash
npx freellama policy-from-eval \
  --aggregate benchmark/local/results/<model>/aggregate.json \
  --task coding --min-pass 0.8 --out platform.toml

npx freellama bench-all --output benchmark-report.json
npx freellama serve --recommendation-catalog recommendations.example.toml
```

`serve` picks up `platform.toml` and `benchmark-report.json` from the working directory
automatically; explicit `--policy-file` / `--benchmark-report` always win. When either is missing it
says so at startup rather than silently grading everything `low`.

`policy-from-eval` refuses to manufacture evidence: fewer than three trials is a smoke result
(`--allow-smoke` marks the output accordingly), aggregates past their review date are rejected, and
models that are not installed are skipped.

