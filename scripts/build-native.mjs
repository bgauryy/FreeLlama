#!/usr/bin/env node
// Builds the N-API addon into packages/mcp/native and writes the generated index.d.ts there.
// The declarations are committed; CI rebuilds and runs `git diff --exit-code` so they never drift.
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const out = path.join(root, "packages/mcp/native");
const args = [
  "napi", "build", "--platform", "--release",
  "--manifest-path", "packages/rust-core/Cargo.toml", "--features", "napi",
  "--no-js", "--dts", "index.d.ts", "-o", out,
];
const result = spawnSync("yarn", args, { cwd: root, stdio: "inherit", shell: process.platform === "win32" });
if (result.status !== 0) process.exit(result.status ?? 1);

// napi-rs writes an empty file when it collects no type definitions (for example a CLI/derive
// major-version mismatch), so require one declaration per `#[napi]` export.
const napiSource = readFileSync(path.join(root, "packages/rust-core/src/napi.rs"), "utf8");
const exported = [...napiSource.matchAll(/#\[napi(?:\([^\]]*\))?\](?:\s*#\[[^\]]*\])*\s*pub (?:async )?fn \w+/g)].length;
const generated = readFileSync(path.join(out, "index.d.ts"), "utf8");
const declared = [...generated.matchAll(/^export declare function \w+/gm)].length;
if (exported === 0 || declared !== exported) {
  console.error(`build-native: index.d.ts declares ${declared} functions but napi.rs exports ${exported}`);
  process.exit(1);
}
