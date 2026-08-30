# freellama-cli

The `freellama` binary. A thin shell over [`freellama-core`](../rust-core/README.md) — it only
touches the library's public API, which is why it lives in its own crate and why the napi build
never has to compile it.

## Getting oriented

```bash
freellama tools     # every MCP tool and the CLI command that does the same thing
freellama doctor    # the only subcommand that runs without `serve`
```

`doctor` first, always: it reports Ollama reachability, CLI/server version drift, your
chip/RAM/disk, and the nine `OLLAMA_*` settings **with their effective defaults**. Unset means
"Ollama picks", not "off", and two of those defaults are commonly wrong for a large-model setup.

## Running the control plane

```bash
freellama serve --recommendation-catalog recommendations.example.toml
```

Then, from another terminal:

```bash
freellama models                                   # capabilities, residency, policy rank
freellama route --task coding --objective fastest  # decision only, no generation
freellama task --task completion "Reply with exactly OK."
freellama bench-all --output benchmark-report.json
```

Every control-plane subcommand needs `serve`. If it is not running they say so and name the command
that starts it, rather than surfacing a raw transport error.

## Making `balanced` / `quality` work

`fastest` needs no configuration. The other objectives — and `minConfidence: "medium"` on the MCP
side — require a routing policy plus a benchmark report:

```bash
freellama policy-from-eval \
  --aggregate benchmark/local/results/<model>/aggregate.json \
  --task coding --min-pass 0.8 --out platform.toml

freellama bench-all --output benchmark-report.json
freellama serve --recommendation-catalog recommendations.example.toml
```

`serve` discovers `platform.toml` and `benchmark-report.json` in the working directory
automatically; `--policy-file` / `--benchmark-report` override that. When either is missing it says
so at startup instead of silently grading every route `low`.

`policy-from-eval` reads **pass rates**, never `bench-all`'s throughput, and refuses to manufacture
evidence: fewer than three trials is a smoke result (`--allow-smoke` marks the output), aggregates
past their review date are rejected, and uninstalled models are skipped.

## What the CLI does not have

`search_models` and `delegate_research` are MCP-only. `freellama tools` says so rather than
pretending parity — and a contract test parses the MCP server's source to make sure that table
cannot drift out of date.
