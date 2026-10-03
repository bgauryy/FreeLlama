#!/usr/bin/env node
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { PLATFORM_PACKAGES, addonName, executableName } from "./release-platforms.mjs";

const root = path.resolve(import.meta.dirname, "..");
const input = path.resolve(process.argv[2] ?? path.join(root, "release-artifacts"));
const output = path.resolve(process.argv[3] ?? path.join(root, "release"));

function contains(ancestor, target) {
  const relative = path.relative(ancestor, target);
  return relative === "" || (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
}

// The output is replaced recursively; it must not contain protected files or overlap inputs.
if (contains(output, root) || contains(output, input) || contains(input, output)
    || PLATFORM_PACKAGES.some(({ id }) => contains(output, path.join(root, "packages", "native", id)))) {
  throw new Error(`unsafe release output: ${output} overlaps inputs or contains the repository or a native package`);
}

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const item = path.join(directory, entry.name);
    return entry.isDirectory() ? walk(item) : [item];
  });
}

// Validate the entire input set before replacing any existing release or native package.
const artifacts = PLATFORM_PACKAGES.map((target) => {
  const directory = path.join(input, target.id);
  if (!existsSync(directory)) throw new Error(`missing release artifact directory: ${directory}`);
  const files = walk(directory);
  const select = (name) => {
    const matches = files.filter((file) => path.basename(file) === name);
    if (matches.length !== 1) throw new Error(`${target.id}: expected exactly one ${name}, found ${matches.length}`);
    const artifact = lstatSync(matches[0]);
    if (!artifact.isFile() || artifact.size === 0) throw new Error(`${target.id}: ${name} must be a nonempty regular file`);
    return matches[0];
  };
  return { target, addon: select(addonName(target.id)), binary: select(executableName(target.id)) };
});

rmSync(output, { recursive: true, force: true });
mkdirSync(output, { recursive: true });

for (const { target, addon, binary } of artifacts) {
  const packageDirectory = path.join(root, "packages", "native", target.id);
  rmSync(path.join(packageDirectory, addonName(target.id)), { force: true });
  rmSync(path.join(packageDirectory, executableName(target.id)), { force: true });
  copyFileSync(addon, path.join(packageDirectory, addonName(target.id)));
  copyFileSync(binary, path.join(packageDirectory, executableName(target.id)));
  if (target.os !== "win32") chmodSync(path.join(packageDirectory, executableName(target.id)), 0o755);
  copyFileSync(binary, path.join(output, `freellama-${target.id}${target.os === "win32" ? ".exe" : ""}`));
  if (target.os !== "win32") chmodSync(path.join(output, `freellama-${target.id}`), 0o755);
}

const checksums = readdirSync(output)
  .sort()
  .map((name) => `${createHash("sha256").update(readFileSync(path.join(output, name))).digest("hex")}  ${name}`)
  .join("\n");
writeFileSync(path.join(output, "SHA256SUMS"), `${checksums}\n`);
console.log(`assembled ${PLATFORM_PACKAGES.length} platform packages and standalone binaries in ${output}`);
