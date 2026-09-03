#!/usr/bin/env node
// Launcher so `npx @octocodeai/freellama` works: finds the compiled Rust binary and hands the process to it.
//
// The binary is platform-specific and compiled by Cargo, so it is shipped in the tarball rather
// than built on install — an install-time `cargo build` would need a Rust toolchain on every
// consumer's machine. On a platform with no shipped binary the error names what it looked for and
// how to build one, instead of a bare ENOENT from deep inside a spawn.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
const requireFromHere = createRequire(import.meta.url);
const PLATFORM_SUFFIXES = process.platform === "linux"
  ? (process.report?.getReport()?.header?.glibcVersionRuntime ? ["-gnu", "-musl"] : ["-musl", "-gnu"])
  : process.platform === "win32" ? ["-msvc"] : [""];
const platformIds = [...new Set(PLATFORM_SUFFIXES.map((suffix) => `${process.platform}-${process.arch}${suffix}`))];
const candidates = [
  path.join(root, "..", "..", "target", "release", exe), // in-repo dev, no packing needed
];

for (const id of platformIds) {
  try {
    candidates.push(path.join(path.dirname(requireFromHere.resolve(`@octocodeai/freellama-native-${id}/package.json`)), exe));
  } catch {
    // Optional packages for other platforms are intentionally absent after install.
  }
}

const binary = candidates.find((p) => existsSync(p));
if (!binary) {
  console.error(
    `freellama: no binary for ${process.platform}-${process.arch}.\n` +
      `Looked in:\n${candidates.map((c) => `  ${c}`).join("\n")}\n` +
      "Build it from a checkout with:\n  cargo build --release\n" +
      "Reinstall without --omit=optional. From a source checkout, run `cargo build --release`.",
  );
  process.exit(1);
}

// Pass through stdio and the exit code: this is a launcher, not a wrapper that reinterprets output.
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

// Forward termination to the child. Without this, a supervisor (or `kill`) that signals the npx
// process by PID kills only this launcher, and `freellama serve` is orphaned still holding its
// listener — the next start then fails with "address already in use" and nothing explains why.
// Ctrl-C already reaches both via the process group; this covers the targeted-signal case.
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}

// Reproduce the shell's own convention for a signalled child (128 + signal number) so callers can
// tell "interrupted" from "failed"; both used to collapse into a bare 1.
child.on("exit", (code, signal) => {
  if (signal) {
    process.exit(128 + (os.constants.signals[signal] ?? 15));
  }
  process.exit(code ?? 0);
});
child.on("error", (err) => {
  console.error(`freellama: failed to start ${binary}: ${err.message}`);
  process.exit(1);
});
