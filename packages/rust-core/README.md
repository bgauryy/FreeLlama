# freellama-core

The whole solution as an embeddable Rust library: model discovery, task-aware routing, session
affinity, admission control, benchmarking, install planning, and a policy generator. The CLI
(`packages/cli`) and the MCP server (`packages/mcp`, via NAPI) are both thin shells over this.

The crate is named `freellama-core`; the library is `freellama`, so consumers write
`use freellama::…`.

## Modules

| Module | Responsibility |
|---|---|
| `platform` | The control plane: `/_freellama/v1/*` routes, model discovery, `select_route`, sessions, managed task execution, admission control |
| `proxy` | Ollama-compatible passthrough with retry, exponential backoff + jitter, per-attempt timeout, and opt-in restart |
| `model_bench` | Throughput benchmarking across installed models (`bench-all`) |
| `recommend` | Side-effect-free install plans from a reviewed catalog — never pulls |
| `policy` | Turns a *quality* benchmark aggregate into a routing policy; see "Evidence" below |
| `napi` | The sole FFI boundary. Feature-gated off by default |
| `lib` | `doctor`, frozen-suite running, and comparison |

## Two ideas worth knowing before reading the code

**Admission control is a lock, not a queue.** `platform::run_task` takes a *shared* read permit when
the selected model is already resident and an *exclusive* write permit when it is not, so a cold
load can never race an active stream. Every HTTP client in the crate sets a timeout, and that is
load-bearing rather than tidy: an untimed request here would hold the exclusive permit forever and
deadlock every subsequent managed task.

**Confidence is earned, not asserted.** `route_evidence` returns `medium` only when a task has both
a configured policy *and* benchmark data:

```
(policy, benchmark) -> medium  configured_task_policy
(policy, -)         -> low     configured_task_policy
(-, benchmark)      -> low     functional_throughput_screen
(-, -)              -> low     capability_metadata_only
```

A policy without measurement is an unverified claim; measurement without a policy is throughput
with nobody vouching for correctness. `policy::qualify_from_aggregate` therefore reads *pass rates*
from a harness aggregate, never `bench-all`'s tokens-per-second — generating a contract from
throughput would relabel speed as quality and make `medium` pass while meaning nothing. It refuses
smoke runs (fewer than three trials), aggregates past their review date, and models that are not
installed.

## Building

```bash
cargo build --release              # library + the CLI in the sibling crate
cargo test                         # contract tests in tests/
cargo clippy --all-targets         # zero warnings expected
```

The Node addon is a separate, feature-gated build — napi's FFI symbols only resolve inside a
running Node process, so the standalone binary must never link them:

```bash
npm --prefix ../.. run build       # -> packages/mcp/native/freellama.<triple>.node
```

`unsafe_code` is `deny` crate-wide; `napi.rs` is the one module that opts out explicitly, because
napi-derive's generated glue requires it.

## Embedding it

```rust
use freellama::platform::{PlatformConfig, serve};

let config = PlatformConfig::new("127.0.0.1:11435", "http://127.0.0.1:11434", None, None, "…")
    .with_recommendation_catalog("recommendations.example.toml");
serve(config).await?;
```

`proxy::serve` runs the passthrough alone. `platform::serve` composes it as a fallback route, so
`serve` is a superset of `proxy` — see `skills/freellama/references/proxy-vs-serve.md`.
