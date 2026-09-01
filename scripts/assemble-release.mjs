#!/usr/bin/env node
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");
const input = path.resolve(process.argv[2] ?? path.join(root, "release-artifacts"));
const output = path.resolve(process.argv[3] ?? path.join(root, "release"));
const native = path.join(root, "packages", "mcp", "native");
const vendor = path.join(root, "packages", "cli", "vendor");
const targets = new Map([
  ["darwin-arm64", { binary: "freellama-darwin-arm64", vendor: "darwin-arm64" }],
  ["darwin-x64", { binary: "freellama-darwin-x64", vendor: "darwin-x64" }],
  ["linux-arm64", { binary: "freellama-linux-arm64", vendor: "linux-arm64" }],
  ["linux-x64", { binary: "freellama-linux-x64", vendor: "linux-x64" }],
  ["win32-x64", { binary: "freellama-win32-x64.exe", vendor: "win32-x64" }],
]);

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const item = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(item) : [item];
  });
}

rmSync(output, { recursive: true, force: true });
rmSync(vendor, { recursive: true, force: true });
mkdirSync(output, { recursive: true });
mkdirSync(vendor, { recursive: true });
for (const file of readdirSync(native)) {
  if (file.endsWith(".node")) rmSync(path.join(native, file));
}

for (const [target, contract] of targets) {
  const directory = path.join(input, target);
  if (!existsSync(directory)) throw new Error(`missing release artifact directory: ${directory}`);
  const files = walk(directory);
  const addon = files.find((file) => file.endsWith(".node"));
  const binary = files.find((file) => path.basename(file) === (target.startsWith("win32") ? "freellama.exe" : "freellama"));
  if (!addon || !binary) throw new Error(`target ${target} must contain one native addon and CLI binary`);
  copyFileSync(addon, path.join(native, path.basename(addon)));
  const vendorDirectory = path.join(vendor, contract.vendor);
  mkdirSync(vendorDirectory, { recursive: true });
  copyFileSync(binary, path.join(vendorDirectory, path.basename(binary)));
  copyFileSync(binary, path.join(output, contract.binary));
  if (!target.startsWith("win32")) chmodSync(path.join(output, contract.binary), 0o755);
}

const checksums = readdirSync(output)
  .sort()
  .map((name) => `${createHash("sha256").update(readFileSync(path.join(output, name))).digest("hex")}  ${name}`)
  .join("\n");
writeFileSync(path.join(output, "SHA256SUMS"), `${checksums}\n`);
console.log(`assembled ${targets.size} release targets in ${output}`);
