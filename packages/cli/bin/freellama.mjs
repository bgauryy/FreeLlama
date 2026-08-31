#!/usr/bin/env node
// Launcher so `npx freellama` works: finds the compiled Rust binary and hands the process to it.
//
// The binary is platform-specific and compiled by Cargo, so it is shipped in the tarball rather
// than built on install — an install-time `cargo build` would need a Rust toolchain on every
// consumer's machine. On a platform with no shipped binary the error names what it looked for and
// how to build one, instead of a bare ENOENT from deep inside a spawn.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
const candidates = [
  path.join(root, "vendor", `${process.platform}-${process.arch}`, exe),
  path.join(root, "..", "..", "target", "release", exe), // in-repo dev, no packing needed
];

const binary = candidates.find((p) => existsSync(p));
if (!binary) {
  console.error(
    `freellama: no binary for ${process.platform}-${process.arch}.\n` +
      `Looked in:\n${candidates.map((c) => `  ${c}`).join("\n")}\n` +
      "Build it from a checkout with:\n  cargo build --release\n" +
      "Prebuilt binaries ship only for the platforms listed in packages/cli/README.md.",
  );
  process.exit(1);
}

// Pass through stdio and the exit code: this is a launcher, not a wrapper that reinterprets output.
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
child.on("exit", (code, signal) => process.exit(signal ? 1 : (code ?? 0)));
child.on("error", (err) => {
  console.error(`freellama: failed to start ${binary}: ${err.message}`);
  process.exit(1);
});
