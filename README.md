# FreeLlama

**An evidence-aware local model gateway for Ollama.**

FreeLlama lets applications and AI agents offload work to local models *without blindly trusting
them*. It discovers what your machine can actually run, routes each task to an installed model, and
admits local execution only when the model and the task have evidence behind them.

**Offload work down. Keep judgment up.**

```
Frontier model      judgment · planning · review
        ↓
FreeLlama           route · admit · verify
        ↓
Local models        research · vision · embeddings · extraction
```

## Why you'd want this

Large files, embedding vectors, images and tool schemas can stay on your machine, while the
expensive orchestrating model receives only the conclusion and the evidence behind it.

**The point is not that it saves tokens — it's that it controls what deserves to enter the expensive
model's context.** Cost follows from that, but so does context quality: the orchestrator never has
to ingest whole files, raw OCR, intermediate research, or repetitive schemas to benefit from them.

Measured on one machine, real calls: **97.1%** of a grounded code question's source never reaches the
orchestrator (59,208 tokens → 1,742 returned); **98.9%** of embedding output stays local; OCR returns
37 tokens for a 1,970-token image. Scaled to a working session that's roughly **605,000 tokens**
preserved — see [`docs/ECONOMICS.md`](docs/ECONOMICS.md) for the full accounting, including what I
discounted and why.

And because a local model is ~99% accurate on grounded lookups but **~67% on judgment, in an
identical confident tone**, nothing is trusted on tone. Every delegated answer carries a verdict
computed from what the run *did* — which files it read, which model ran — never from what the model
says about itself.

## Run it

```bash
cargo build --release                  # freellama-core + the freellama CLI
npm install && npm run build           # native addon
npm --prefix packages/mcp install && npm --prefix packages/mcp run build

./target/release/freellama doctor      # works with nothing else running
./target/release/freellama serve       # the gateway, on 127.0.0.1:11435
```

You need Rust 1.85+, Node 20+, a running Ollama, and one installed model of ~12B or larger — below
that, accuracy on research collapses.

`.mcp.json` already registers the MCP server, so an MCP-capable agent in this repo picks it up with
no setup.

## Inspectable routing

`confidence: "medium"` on its own reads like a calibrated probability. It isn't one — so the
dimensions it's derived from are reported separately:

```
$ freellama route --task code-repair --objective fastest
  selected        : qwen3.8:27b-mlx
  qualityEvidence : none          # no policy vouches for this model on this task
  taskEvidence    : none          # no functional benchmark measured it
  hardwareFit     : strong        # fits the requested context
  confidence      : low           # derived from the above, not asserted
  rejected        : []            # every losing candidate, with its reason
```

Pass `--min-confidence medium` and an unjustified route is **refused**, naming the grade, the
evidence behind it, the model it would have picked, and the two commands that would raise it. The
gate lives in the router itself, so the CLI, the HTTP API and anyone embedding `freellama-core`
inherit it.

## Documentation

| Doc | What's in it |
|---|---|
| [Architecture](docs/ARCHITECTURE.md) | the three tiers, admission and throttling, request paths, product boundary |
| [Economics](docs/ECONOMICS.md) | the token accounting in full, with the discounts stated |
| [CLI](docs/CLI.md) | every subcommand, policy generation, objectives |
| [Model selection](docs/MODEL_SELECTION.md) | which local models, and how to re-derive it for your machine |
| [Testing](docs/TESTING.md) | the suites and what each one asserts |
| [Ollama sidecar](docs/OLLAMA_SIDECAR.md) | why the proxy is a sidecar, not a plugin |
| [System optimization](docs/OLLAMA_SYSTEM_OPTIMIZATION.md) | what FreeLlama tunes, and what it deliberately doesn't |
| [FreeLlama skill](skills/freellama/SKILL.md) | the orchestration playbook for an agent driving this |
| [Agents](AGENTS.md) | the adapter loop, context management, benchmark adapters |
| [Benchmarks](benchmark/harness/README.md) | the three benchmark surfaces and when to use which |

## What it is not

Not a remote provider marketplace, billing layer, model registry, installation executor, agent
runtime, or A2A coordinator. **It also does not make inference faster** — a 2026-08-23 audit traced
a measured 43.9% speedup entirely to Ollama's MLX artifact, not to FreeLlama; holding the artifact
constant, the proxy added 0.330 ms and no speedup. Use Ollama directly if raw speed for one exact
model is the goal.

Ollama runs models. FreeLlama decides when local inference deserves to be used, which model gets the
work, and what evidence is allowed back into the orchestrator.

## Security

The platform binds to loopback and has no authentication layer. Keep it local. Do not expose port
`11435` through a public listener or reverse proxy without adding authentication, authorization,
TLS, rate limits, and tenant isolation.

Licensed `Apache-2.0 OR MIT`.
