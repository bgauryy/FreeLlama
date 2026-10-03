# Release

FreeLlama ships through two parallel channels from a single build:

- **npm** — `@octocodeai/freellama` (CLI launcher), `@octocodeai/freellama-mcp-server` (MCP server),
  and eight `@octocodeai/freellama-native-<platform>` optional dependencies containing the compiled
  Rust artifacts. Node 20+ consumers get everything through `npm install`.
- **GitHub Releases** — standalone platform binaries for users who do not have Node installed.
  `scripts/install.sh` downloads the right binary, verifies its SHA-256, and places it in
  `~/.local/bin/freellama`.

The Rust crates (`freellama-core`, `freellama-cli`) are internal implementation crates and are
never published to crates.io (`publish = false`).

---

## Platform matrix

Every release covers exactly these eight targets, defined once in
[`scripts/release-platforms.mjs`](scripts/release-platforms.mjs):

| npm package id | OS | CPU | Rust target |
|---|---|---|---|
| `darwin-arm64` | macOS | Apple Silicon | `aarch64-apple-darwin` |
| `darwin-x64` | macOS | Intel | `x86_64-apple-darwin` |
| `linux-arm64-gnu` | Linux | arm64 | `aarch64-unknown-linux-gnu` |
| `linux-arm64-musl` | Linux | arm64 (musl) | `aarch64-unknown-linux-musl` |
| `linux-x64-gnu` | Linux | x64 | `x86_64-unknown-linux-gnu` |
| `linux-x64-musl` | Linux | x64 (musl) | `x86_64-unknown-linux-musl` |
| `win32-arm64-msvc` | Windows | arm64 | `aarch64-pc-windows-msvc` |
| `win32-x64-msvc` | Windows | x64 | `x86_64-pc-windows-msvc` |

Each platform package contains two runtime artifacts and the Apache and MIT license texts:

- `freellama[.exe]` — the standalone CLI binary
- `freellama.<id>.node` — the N-API addon loaded by the MCP server

---

## Prerequisites

```bash
rustup show          # Rust 1.85+
node --version       # Node 20+
yarn --version       # Yarn 4+
npx napi --version   # napi-rs CLI 3+ (devDependency, auto-available)
```

All eight Rust targets must be installed:

```bash
rustup target add \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  aarch64-unknown-linux-musl \
  x86_64-unknown-linux-gnu \
  x86_64-unknown-linux-musl \
  aarch64-pc-windows-msvc \
  x86_64-pc-windows-msvc
```

Cross-compilation from macOS (the primary build host) requires:

| Target family | Tool | Install |
|---|---|---|
| `darwin-*` | Apple Clang (native) | Ships with Xcode |
| `linux-*` | cargo-zigbuild + zig | `cargo install cargo-zigbuild && brew install zig` |
| `win32-*-msvc` | cargo-xwin | `cargo install cargo-xwin` |

---

## Prepare the release version

Complete the [version bump checklist](#version-bump-checklist) before building artifacts.
The current release candidate is `0.2.1`; use `v0.2.1` for its release tag.
Refresh both lockfiles after updating the manifests:

```bash
yarn install
cargo update --workspace
```

Review `yarn.lock` and `Cargo.lock` with the version changes. Rebuild all eight platform artifact
pairs after a version bump; a manifest change does not update the version embedded in an existing
CLI binary. Then assemble the rebuilt artifacts and run the package gate below.

Check production dependencies before publication:

```bash
yarn npm audit --all --recursive --environment production
```

Review any findings, update compatible dependencies, and rerun the production gate after a lockfile change.

## Step 1 — Cross-compile all platforms

For each of the eight targets, produce two artifacts and stage them under
`release-artifacts/<id>/`:

```bash
mkdir -p release-artifacts/{darwin-arm64,darwin-x64,linux-arm64-gnu,linux-arm64-musl,linux-x64-gnu,linux-x64-musl,win32-arm64-msvc,win32-x64-msvc}
```

**macOS targets** — Apple Clang cross-compiles x64 from arm64 natively:

```bash
# darwin-arm64
npx napi build --platform --target aarch64-apple-darwin --release \
  --manifest-path packages/rust-core/Cargo.toml --features napi \
  --no-js --no-dts-header -o release-artifacts/darwin-arm64/
cargo build --release --target aarch64-apple-darwin -p freellama-cli
cp target/aarch64-apple-darwin/release/freellama release-artifacts/darwin-arm64/

# darwin-x64
npx napi build --platform --target x86_64-apple-darwin --release \
  --manifest-path packages/rust-core/Cargo.toml --features napi \
  --no-js --no-dts-header -o release-artifacts/darwin-x64/
cargo build --release --target x86_64-apple-darwin -p freellama-cli
cp target/x86_64-apple-darwin/release/freellama release-artifacts/darwin-x64/
```

**Linux targets** — via cargo-zigbuild (zig handles both glibc and musl):

```bash
for TARGET in \
  x86_64-unknown-linux-gnu:linux-x64-gnu \
  aarch64-unknown-linux-gnu:linux-arm64-gnu \
  x86_64-unknown-linux-musl:linux-x64-musl \
  aarch64-unknown-linux-musl:linux-arm64-musl
do
  RUST="${TARGET%%:*}" ; ID="${TARGET##*:}"
  npx napi build --platform --cross-compile --target "$RUST" --release \
    --manifest-path packages/rust-core/Cargo.toml --features napi \
    --no-js --no-dts-header -o "release-artifacts/$ID/"
  cargo zigbuild --release --target "$RUST" -p freellama-cli
  cp "target/$RUST/release/freellama" "release-artifacts/$ID/"
done
```

**Windows targets** — via cargo-xwin (downloads the Windows SDK on first use, ~1.5 GB cached):

```bash
for TARGET in \
  x86_64-pc-windows-msvc:win32-x64-msvc \
  aarch64-pc-windows-msvc:win32-arm64-msvc
do
  RUST="${TARGET%%:*}" ; ID="${TARGET##*:}"
  npx napi build --platform --cross-compile --target "$RUST" --release \
    --manifest-path packages/rust-core/Cargo.toml --features napi \
    --no-js --no-dts-header -o "release-artifacts/$ID/"
  cargo xwin build --release --target "$RUST" -p freellama-cli
  cp "target/$RUST/release/freellama.exe" "release-artifacts/$ID/"
done
```

Each `release-artifacts/<id>/` directory must contain exactly:
- `freellama.<id>.node` (N-API addon, built by napi with `--features napi`)
- `freellama` or `freellama.exe` (CLI binary, built without `--features napi`)

---

## Step 2 — Assemble release artifacts

```bash
yarn release:assemble
# node scripts/assemble-release.mjs
```

This script reads every `release-artifacts/<id>/` directory and copies artifacts to two places:

| Destination | Purpose |
|---|---|
| `packages/native/<id>/freellama[.exe]` | npm publish artifact |
| `packages/native/<id>/freellama.<id>.node` | npm publish artifact |
| `assets/logo.jpg` in all ten package directories | npm publish artifact, copied from `assets/logo.jpg` |
| `release/freellama-<id>[.exe]` | GitHub Release download |
| `release/SHA256SUMS` | Verified by `scripts/install.sh` |

Both `release-artifacts/` and `release/` are git-ignored build outputs.

---

## Step 3 — Run the full pre-flight check

```bash
yarn verify:production
```

This runs the complete gate in order:

```
yarn build                                        # rebuild everything from source
cargo fmt --all --check                           # formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings
yarn test:all                                     # typecheck + unit + Rust + agent + hardware + release + integration + e2e
yarn release:verify:publish                       # strict artifact check (see below)
```

The strict artifact check (`FREELLAMA_REQUIRE_ALL_PLATFORMS=1`) calls `npm pack --dry-run` on all
10 packages and verifies that every required file exists, is non-empty, and appears in the
dry-run tarball listing. Every tarball must include `assets/logo.jpg`. The check also confirms both
Rust crates declare `publish = false`. `yarn build:licenses` copies that image from
`assets/logo.jpg` into the CLI package, the MCP package, and all eight native packages.

All nine promotion conditions from [`docs/PRODUCTION.md`](docs/PRODUCTION.md)
must pass before continuing.

After the package gate passes, check the registry before publishing any package:

```bash
yarn release:verify:publish
yarn release:verify:registry
```

The registry preflight checks all ten package names at the candidate workspace version.
The check fails if any exact version already exists or the registry response
cannot establish absence. For `0.2.1`, all ten versions must be absent before the first publish.
This check does not prove that your npm account can publish, that registry credentials work,
or that a later publication cannot race with another publisher. Obtain release authorization and
verify the publishing account separately. Repeat the preflight immediately before publication.

---

## Step 4 — Publish to npm

**Order is mandatory.** The portable packages (`@octocodeai/freellama` and
`@octocodeai/freellama-mcp-server`) declare all eight native packages as
`optionalDependencies`. npm resolves them at install time, so they must exist in the registry
before the portable packages are published.

### 4a — Publish the eight native platform packages

```bash
for dir in ./packages/native/*/; do
  npm publish "$dir" --access public
done
```

Each package is published as `@octocodeai/freellama-native-<id>@<version>`.

### 4b — Publish the CLI launcher

```bash
npm publish ./packages/cli --access public
```

Publishes `@octocodeai/freellama@<version>`. The package contains the JS launcher
(`bin/freellama.js`), `assets/logo.jpg`, and the package README. It contains no binary. The
matching native package is pulled at install time via `optionalDependencies`.

### 4c — Publish the MCP server

```bash
npm publish ./packages/mcp --access public
```

Publishes `@octocodeai/freellama-mcp-server@<version>`. The `prepublishOnly` hook runs
`yarn typecheck && yarn build && yarn test` automatically. The package contains
`dist/index.js`, the `native/` loader shim, bundled Python adapters, documentation, and
`assets/logo.jpg`.
The `.node` binary is not embedded — it arrives via `optionalDependencies` the same way.

### Dry-run before publishing

```bash
npm pack --dry-run --json ./packages/native/darwin-arm64  # inspect one native package
npm pack --dry-run ./packages/mcp                         # inspect the MCP server
npm pack --dry-run ./packages/cli                         # inspect the CLI launcher
```

---

## Step 5 — Create a GitHub release

Upload every file in `release/` as a release asset:

```
release/freellama-darwin-arm64
release/freellama-darwin-x64
release/freellama-linux-arm64-gnu
release/freellama-linux-arm64-musl
release/freellama-linux-x64-gnu
release/freellama-linux-x64-musl
release/freellama-win32-arm64-msvc.exe
release/freellama-win32-x64-msvc.exe
release/SHA256SUMS
```

The release tag must match the `version` field in the root `package.json` and `Cargo.toml`
(for example, `v0.2.1`). `scripts/install.sh` constructs the download URL from the tag:

```bash
scripts/install.sh --version vX.Y.Z --bin-dir ~/.local/bin
```

Replace `vX.Y.Z` with the published release tag.

It detects the host platform and architecture, downloads the matching binary, verifies its
SHA-256 against `SHA256SUMS`, and installs it.

---

## Version bump checklist

All version fields are kept in sync manually before cutting a release:

- `version` in root `package.json`
- `version` under `[workspace.package]` in root `Cargo.toml` (inherited by both Rust crates)
- `version` in all `packages/native/*/package.json` (8 files)
- `version` in `packages/mcp/package.json`
- `version` in `packages/cli/package.json`
- `"<version>"` in the `optionalDependencies` of `packages/mcp/package.json` and
  `packages/cli/package.json` (must match the new version exactly)

The `verify:production` gate checks that every optional dependency in both portable packages
points to the current workspace version and errors if any diverge.
Refresh both lockfiles and rebuild the versioned artifacts before running that gate.
Run `yarn release:verify:registry` before any publication; never reuse an already published version.

---

## What each consumer installs

| Consumer | Command | Receives |
|---|---|---|
| MCP client (Cursor, Claude Desktop, etc.) | `npx @octocodeai/freellama-mcp-server` | MCP server bundle + `.node` addon via optional dep |
| Node CLI user | `npx @octocodeai/freellama` | JS launcher + CLI binary via optional dep |
| Shell user (no Node) | `scripts/install.sh --version vX.Y.Z` | Standalone CLI binary from GitHub Release |

Do not install with `--omit=optional`. The matching native package supplies the CLI binary and
MCP addon; omitting it leaves the JavaScript entry points without their runtime artifacts.

---

## Automated workflows

[CI](.github/workflows/ci.yml) runs on pull requests and pushes to `main`. It checks Rust formatting,
Clippy, and tests on Linux, macOS, and Windows. Separate jobs build the native addon, check TypeScript,
run JavaScript tests and package verification, audit production dependencies, and test the Python adapters.

[Release](.github/workflows/release.yml) builds all eight platform targets on version tags or manual
dispatch. Before uploading each target's artifacts, its build lane checks the CLI's reported version
against the workspace version and loads the N-API addon. Non-musl addons load on the target host;
musl addons load in the `node:22-alpine` container. Each load check requires the `machine` binding.
These smoke checks require no model inference. Confirm successful remote receipts for the candidate
revision; inspecting the workflow source alone does not prove target execution.

Its publish job assembles artifacts, builds the MCP package, runs TypeScript and JavaScript
checks, verifies all publish artifacts, and checks all ten candidate versions in the registry before
any npm publication. Version-tag runs publish native packages before the CLI
and MCP packages, then upload GitHub Release assets. Manual dispatch builds and verifies without
publishing. npm publication requires the configured `NPM_TOKEN` secret and uses `--provenance`.

These workflows do not run the full live integration and end-to-end matrix in `verify:production`.
Complete that gate and the production promotion conditions before creating a release tag.


The release workflow requires the reusable CI checks before building artifacts.
CI includes the hardware harness and packaging contracts, in addition to Rust and JavaScript checks.
The publish verifier rejects a version tag that differs from the workspace version.
Live E2E and physical hardware promotion remain separate operator checks.
Root `LICENSE-APACHE` and `LICENSE-MIT` are the canonical license texts.
Builds copy both texts into every published package.
