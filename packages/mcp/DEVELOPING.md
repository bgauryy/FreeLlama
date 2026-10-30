# Developing the FreeLlama MCP server

Build, test, and release notes for the [FreeLlama MCP server](README.md).

## Build

Build the native addon from the repository root and this package in place.

```bash
# From the repository root (yarn workspaces — one install for every package)
corepack enable
yarn install
yarn build
```

`yarn build` writes **one** MCP file, `packages/mcp/dist/index.js` (shebang, executable). During
checkout development it loads `native/freellama.<triple>.node`. Published installs resolve the
matching `@octocodeai/freellama-native-<target>` optional package instead, so the base MCP package remains
portable. Research adapters are copied into `adapters/` for packed installs.

Piecewise, if you are iterating on one layer:

```bash
# 1. Native addon → packages/mcp/native/freellama.<triple>.node
yarn build:native

# 2. This server → dist/index.js via esbuild (then copies the research adapters into adapters/)
yarn workspace @octocodeai/freellama-mcp-server build
```

`yarn start` needs `dist/index.js` and the compiled `.node` addon — `yarn build` from the repository
root produces both in a checkout. `dist/index.js` loads `native/index.js`, which loads a local addon
first and then a matching optional platform package. The `.node` binary is gitignored (a build
artifact). `native/index.js` is a small hand-written loader; `native/index.d.ts` is generated from
`napi.rs` by `yarn build:native`. Both are checked in; do not edit the declarations by hand —
change the Rust doc comments or `ts_args_type`, rebuild, and commit. CI rebuilds it and fails
on any diff.

## Test

Vitest, in three tiers (all TypeScript, under `test/`):

| Tier | Command (from this package) | Checks | Needs |
|---|---|---|---|
| `test/unit/` | `yarn test` (watch: `yarn test:watch`) | pure functions straight from `src/*.ts` — no build step, this is the TDD loop | nothing, ~0.5s |
| `test/integration/` | `yarn test:integration` | the built server over the real MCP protocol: tool contract, structured content, guardrails; rebuilds `dist/` itself | Ollama at `FREELLAMA_OLLAMA_ENDPOINT` (default :11434) |
| `test/e2e/` | `yarn test:e2e` | every tool against the live system: real routing/execution, pull → delete round trip (net-zero), a real delegated research run | serve + Ollama + models, ~40s |

The same commands work from the repository root (`yarn test` there also runs the CLI package's tests).
The integration/e2e tiers fail fast with a readable message when Ollama is down, and the lifecycle
test refuses to delete a model already installed on the test system.

For default-endpoint autostart, a spawn failure, early exit, or readiness timeout rejects the
request while leaving the MCP transport available for diagnosis or retry. Autostart does not apply
to an explicit non-default endpoint; see `FREELLAMA_MCP_AUTOSTART_SERVE` in
[configuration](CONFIGURATION.md#environment-variables).

## Platform support

The native addon is named by target triple (`freellama.<platform>-<arch>[-<abi>].node`). The base
package has eight npm optional dependencies, one each for macOS arm64/x64, Linux arm64/x64 with
glibc or musl, and Windows arm64/x64 with MSVC. `native/index.js` tries the checkout artifact first,
then the host's optional package. A release is supported only when all eight artifact packages pass
the publish verifier; an installation made with `--omit=optional` cannot load the addon. `engines`
requires Node >= 20.

Machine discovery distinguishes total host memory (`memory_bytes`) from known unified memory
(`unified_memory_bytes`). Library-tag `fitsInMemory` is a conservative host-memory preflight and
includes `fitScope: "host_memory_budget_only"`; inspect `/api/ps` for actual accelerator placement.

Start the serve-backed tools' dependency from the repository root:

```bash
cargo run --release -- serve
```

See `skills/freellama/references/proxy-vs-serve.md` for the `proxy` vs `serve` distinction (only
`serve` has the `/_freellama/v1/*` routes these tools call).

## Publish

`packages/mcp/` is the portable JavaScript package. Its `package.json` lists the platform packages
as exact-version optional dependencies; it intentionally does not embed a `.node` binary. Publish
the eight `packages/native/*` packages first, then publish `@octocodeai/freellama` (`npx @octocodeai/freellama`) and `@octocodeai/freellama-mcp-server` (`npx @octocodeai/freellama-mcp-server`)
at the same version. `yarn release:verify:publish` refuses a release if any of the ten packages
omits `assets/logo.jpg`, or if any platform package has a missing, empty, or unpacked
executable/addon pair.

```bash
cd packages/mcp
npm pack --dry-run   # confirm exactly what a publish would ship
```

Treat `npm publish` as a public release action. Follow the repository
[release procedure](https://github.com/bgauryy/FreeLlama/blob/main/RELEASE.md) and obtain the repository owner's release authorization first.

## Why native bindings

- **No routing reimplementation.** `run_task`, `run_task {preview:true}`, and the installed-model list are HTTP
  calls into a running `freellama serve`, matching the CLI tool map in `packages/cli/src/tools.rs`. `doctor` talks to Ollama
  and the host's portable machine-discovery layer directly. `ollama_manage` / `ollama_delete` hit Ollama's HTTP API. `delegate_research`
  is a subprocess. Routing decisions still live in exactly one place — the Rust core; see
  [what the MCP layer computes](README.md#what-the-mcp-layer-computes).
- **One connection pool.** `packages/rust-core/src/napi.rs` holds a single `reqwest::Client` in a
  `OnceLock` and clones the handle per request, matching the server side.
- **`unsafe_code` stays isolated.** The crate is `deny(unsafe_code)` except `napi.rs`, which
  carries an explicit `#[allow(unsafe_code)]` for napi-derive's FFI glue.
- **The napi build is off by default** (`cargo build` never touches it); the addon is built with
  `napi build --features napi` from the repo root.
