import { readFileSync } from "node:fs";

// Both Rust artifacts embed CARGO_PKG_VERSION. A manifest bump does not change an existing build,
// so a stale artifact lacks the workspace version bytes. Presence is necessary, not sufficient;
// the release workflow additionally runs `freellama --version` on each target host.
export function requireEmbeddedVersion(file, version) {
  if (!readFileSync(file).includes(Buffer.from(version))) {
    throw new Error(`${file}: does not embed version ${version}; rebuild it (yarn release:build)`);
  }
}

export const SOURCE_MARKER_PREFIX = "freellama-source-commit=";
const REBUILD = "yarn release:build -> node scripts/assemble-release.mjs release-artifacts release";

// The commit a release artifact was built from, as embedded by packages/rust-core/build.rs
// (`freellama-source-commit=<sha>[-dirty]` or `unknown`); null when the marker is absent.
export function embeddedSourceCommit(file) {
  const match = /freellama-source-commit=([0-9a-f]{40}(?:-dirty)?|unknown)/.exec(readFileSync(file).toString("latin1"));
  return match ? match[1] : null;
}

// Publish gate: the artifact must come from exactly the release commit, with a clean tracked tree.
export function requireSourceCommit(file, expectedCommit) {
  const built = embeddedSourceCommit(file);
  if (built === null) {
    throw new Error(`${file}: stale artifact, no ${SOURCE_MARKER_PREFIX} marker (built from older source); rebuild it: ${REBUILD}`);
  }
  if (built.endsWith("-dirty")) {
    throw new Error(`${file}: stale artifact, built from a dirty tree (${built}); commit first, then rebuild: ${REBUILD}`);
  }
  if (built !== expectedCommit) {
    throw new Error(`${file}: stale artifact, built from commit ${built} but the release commit is ${expectedCommit}; rebuild it: ${REBUILD}`);
  }
}
