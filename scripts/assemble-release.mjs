#!/usr/bin/env node
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { PLATFORM_PACKAGES, addonName, executableName } from "./release-platforms.mjs";

const root = path.resolve(import.meta.dirname, "..");
const input = path.resolve(process.argv[2] ?? path.join(root, "release-artifacts"));
const output = path.resolve(process.argv[3] ?? path.join(root, "release"));

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const item = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(item) : [item];
  });
}

rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });

for (const target of PLATFORM_PACKAGES) {
  const directory = path.join(input, target.id);
  if (!existsSync(directory)) throw new Error(`missing release artifact directory: ${directory}`);
  const files = walk(directory);
  const addon = files.find((file) => path.basename(file) === addonName(target.id));
  const binary = files.find((file) => path.basename(file) === executableName(target.id));
  if (!addon || !binary) throw new Error(`${target.id}: expected ${addonName(target.id)} and ${executableName(target.id)}`);
  const packageDirectory = path.join(root, "packages", "native", target.id);
  rmSync(path.join(packageDirectory, addonName(target.id)), { force: true });
  rmSync(path.join(packageDirectory, executableName(target.id)), { force: true });
  copyFileSync(addon, path.join(packageDirectory, addonName(target.id)));
  copyFileSync(binary, path.join(packageDirectory, executableName(target.id)));
  copyFileSync(binary, path.join(output, `freellama-${target.id}${target.os === "win32" ? ".exe" : ""}`));
  if (target.os !== "win32") chmodSync(path.join(output, `freellama-${target.id}`), 0o755);
}

const checksums = readdirSync(output)
  .sort()
  .map((name) => `${createHash("sha256").update(readFileSync(path.join(output, name))).digest("hex")}  ${name}`)
  .join("\n");
writeFileSync(path.join(output, "SHA256SUMS"), `${checksums}\n`);
console.log(`assembled ${PLATFORM_PACKAGES.length} platform packages and standalone binaries in ${output}`);
