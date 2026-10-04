import { readFileSync } from "node:fs";

// Both Rust artifacts embed CARGO_PKG_VERSION. A manifest bump does not change an existing build,
// so a stale artifact lacks the workspace version bytes. Presence is necessary, not sufficient;
// the release workflow additionally runs `freellama --version` on each target host.
export function requireEmbeddedVersion(file, version) {
  if (!readFileSync(file).includes(Buffer.from(version))) {
    throw new Error(`${file}: does not embed version ${version}; rebuild it (yarn release:build)`);
  }
}
