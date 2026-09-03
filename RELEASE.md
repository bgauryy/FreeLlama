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

Each platform package contains exactly two artifacts:

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
yarn test:all                                     # typecheck + unit + Rust + agent + integration + e2e
yarn release:verify:publish                       # strict artifact check (see below)
```

The strict artifact check (`FREELLAMA_REQUIRE_ALL_PLATFORMS=1`) calls `npm pack --dry-run` on all
10 packages and verifies that every required file exists, is non-empty, and would be included in
the published tarball. It also confirms both Rust crates declare `publish = false`.

All nine promotion conditions from [`packages/mcp/docs/PRODUCTION.md`](packages/mcp/docs/PRODUCTION.md)
must pass before continuing.

---

## Step 4 — Publish to npm

**Order is mandatory.** The portable packages (`@octocodeai/freellama` and
`@octocodeai/freellama-mcp-server`) declare all eight native packages as
`optionalDependencies`. npm resolves them at install time, so they must exist in the registry
before the portable packages are published.

### 4a — Publish the eight native platform packages

```bash
for dir in packages/native/*/; do
  npm publish "$dir" --access public
done
```

Each package is published as `@octocodeai/freellama-native-<id>@<version>`.

### 4b — Publish the CLI launcher

```bash
npm publish packages/cli --access public
```

Publishes `@octocodeai/freellama@<version>`. The package contains only the JS launcher
(`bin/freellama.js`) — no binary. The matching native package is pulled at install time via
`optionalDependencies`.

### 4c — Publish the MCP server

```bash
npm publish packages/mcp --access public
```

Publishes `@octocodeai/freellama-mcp-server@<version>`. The `prepublishOnly` hook runs
`yarn typecheck && yarn build && yarn test` automatically. The package contains
`dist/index.js`, the `native/` loader shim, bundled Python adapters, and documentation.
The `.node` binary is not embedded — it arrives via `optionalDependencies` the same way.

### Dry-run before publishing

```bash
npm pack --dry-run --json packages/native/darwin-arm64   # inspect one native package
npm pack --dry-run packages/mcp                          # inspect the MCP server
npm pack --dry-run packages/cli                          # inspect the CLI launcher
```

---

## Step 5 — Create a GitHub Release

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
(e.g. `v0.1.0`). `scripts/install.sh` constructs the download URL from the tag:

```bash
scripts/install.sh --version v0.1.0 [--bin-dir ~/.local/bin]
```

It detects the host platform and architecture, downloads the matching binary, verifies its
SHA-256 against `SHA256SUMS`, and installs it.

---

## Version bump checklist

All version fields are kept in sync manually before cutting a release:

- `version` in root `package.json`
- `version.workspace` in root `Cargo.toml` (propagates to both Rust crates)
- `version` in all `packages/native/*/package.json` (8 files)
- `version` in `packages/mcp/package.json`
- `version` in `packages/cli/package.json`
- `"<version>"` in the `optionalDependencies` of `packages/mcp/package.json` and
  `packages/cli/package.json` (must match the new version exactly)

The `verify:production` gate checks that every optional dependency in both portable packages
points to the current workspace version and errors if any diverge.

---

## What each consumer installs

| Consumer | Command | Receives |
|---|---|---|
| MCP client (Cursor, Claude Desktop, etc.) | `npx @octocodeai/freellama-mcp-server` | MCP server bundle + `.node` addon via optional dep |
| Node CLI user | `npx @octocodeai/freellama` | JS launcher + `.node` addon via optional dep |
| Shell user (no Node) | `scripts/install.sh --version vX.Y.Z` | Single static binary from GitHub Release |

Do not install with `--omit=optional`. The native addon is the runtime; omitting it leaves only
the JS shim with nothing to load.

---

## No automated CI yet

There are no `.github/workflows/` files. The steps above are currently run manually by the
repository owner. The natural next additions are:

- A **release workflow** that runs `verify:production`, assembles artifacts, publishes the 10
  npm packages with `--provenance` (using the GitHub Actions OIDC token), and uploads
  `release/` assets to a GitHub Release.
- A **PR check workflow** that runs `yarn test:all` and `cargo clippy` on every pull request.
