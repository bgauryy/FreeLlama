#!/usr/bin/env node
// Sets one release version everywhere it is declared: every workspace package.json, the native
// optionalDependencies pins, and [workspace.package] in Cargo.toml. Lockfiles are refreshed after.
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { PLATFORM_PACKAGES, nativePackageName } from "./release-platforms.mjs";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  throw new Error("usage: yarn release:version <semver>, e.g. 0.6.0");
}
const root = path.resolve(import.meta.dirname, "..");
const manifests = [
  "package.json", "packages/cli/package.json", "packages/mcp/package.json", "packages/view/package.json",
  ...PLATFORM_PACKAGES.map(({ id }) => `packages/native/${id}/package.json`),
];

for (const relative of manifests) {
  const file = path.join(root, relative);
  const manifest = JSON.parse(readFileSync(file, "utf8"));
  manifest.version = version;
  for (const { id } of PLATFORM_PACKAGES) {
    if (manifest.optionalDependencies?.[nativePackageName(id)]) manifest.optionalDependencies[nativePackageName(id)] = version;
  }
  writeFileSync(file, `${JSON.stringify(manifest, null, 2)}\n`);
}

const cargoFile = path.join(root, "Cargo.toml");
const cargo = readFileSync(cargoFile, "utf8");
const updated = cargo.replace(/(\[workspace\.package\][^[]*?\nversion = )"[^"]*"/, `$1"${version}"`);
if (updated === cargo && !cargo.includes(`version = "${version}"`)) throw new Error("Cargo.toml: [workspace.package] version not found");
writeFileSync(cargoFile, updated);

execFileSync("yarn", ["install"], { cwd: root, stdio: "inherit" });
execFileSync("cargo", ["update", "--workspace"], { cwd: root, stdio: "inherit" });
console.log(`set ${manifests.length} manifests and Cargo.toml to ${version}; next: yarn release:build`);
