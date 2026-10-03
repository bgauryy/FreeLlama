#!/usr/bin/env node
// Release gate for the portable JS packages and every optional native package.
// Set FREELLAMA_REQUIRE_ALL_PLATFORMS=1 after `yarn release:assemble`; that is
// the publish gate and refuses an empty or unpacked native target.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { PLATFORM_PACKAGES, addonName, executableName, nativePackageName } from "./release-platforms.mjs";

const requireAllPlatforms = process.env.FREELLAMA_REQUIRE_ALL_PLATFORMS === "1";
const root = JSON.parse(readFileSync("package.json", "utf8"));
const version = root.version;
const releaseTag = process.env.FREELLAMA_RELEASE_TAG;
if (releaseTag && releaseTag !== `v${version}`) throw new Error(`release tag ${releaseTag} must match workspace version v${version}`);
const publicPackages = new Map([
  ["packages/cli", "@octocodeai/freellama"],
  ["packages/mcp", "@octocodeai/freellama-mcp-server"],
]);

function pack(directory) {
  // npm.cmd requires the Windows command interpreter; keep its command literal.
  const command = process.platform === "win32" ? "cmd.exe" : "npm";
  const args = process.platform === "win32"
    ? ["/d", "/s", "/c", "npm.cmd pack --dry-run --json --ignore-scripts"]
    : ["pack", "--dry-run", "--json", "--ignore-scripts"];
  const output = execFileSync(command, args, {
    cwd: directory,
    encoding: "utf8",
  });
  const report = JSON.parse(output)[0];
  if (!report?.files?.length) throw new Error(`${directory}: npm pack contains no files`);
  return new Set(report.files.map((file) => file.path));
}

const heroImage = "assets/logo.jpg";

function requireLicenses(directory, paths) {
  for (const file of ["LICENSE-APACHE", "LICENSE-MIT", heroImage]) requirePackedFile(directory, paths, file);
}

function manifest(directory) {
  return JSON.parse(readFileSync(path.join(directory, "package.json"), "utf8"));
}

function requirePackedFile(directory, paths, file) {
  const artifact = path.join(directory, file);
  if (!existsSync(artifact) || !statSync(artifact).isFile() || statSync(artifact).size === 0 || !paths.has(file)) {
    throw new Error(`${artifact}: required runtime file is missing, empty, or excluded from npm pack`);
  }
}

for (const [directory, expectedName] of publicPackages) {
  const packageManifest = manifest(directory);
  const paths = pack(directory);
  requireLicenses(directory, paths);
  if (packageManifest.name !== expectedName || packageManifest.publishConfig?.access !== "public") {
    throw new Error(`${directory}: must publish publicly as ${expectedName}`);
  }
  if (packageManifest.version !== version) {
    throw new Error(`${directory}: version ${packageManifest.version} must match workspace version ${version}`);
  }
  if (packageManifest.os || packageManifest.cpu) {
    throw new Error(`${directory}: portable launcher package must not restrict os/cpu`);
  }
  if (!paths.has("package.json")) throw new Error(`${directory}: package.json missing from npm pack`);
  if (directory === "packages/cli") {
    requirePackedFile(directory, paths, "bin/freellama.js");
    if ([...paths].some((file) => file.startsWith("vendor/"))) {
      throw new Error(`${directory}: CLI binaries belong only in optional platform packages`);
    }
  } else {
    for (const file of [
      "dist/index.js", "native/index.js", "native/index.d.ts", "native/package.json",
      "adapters/agent_context.py", "adapters/agent_transport.py", "adapters/shell_sandbox.py",
      "adapters/bash_agent.py", "adapters/octocode_agent.py",
    ]) {
      requirePackedFile(directory, paths, file);
    }
    if ([...paths].some((file) => file.endsWith(".node"))) {
      throw new Error(`${directory}: MCP base package must not embed a platform addon`);
    }
  }
  const dependencies = packageManifest.optionalDependencies ?? {};
  for (const target of PLATFORM_PACKAGES) {
    if (dependencies[nativePackageName(target.id)] !== version) {
      throw new Error(`${directory}: optional dependency ${nativePackageName(target.id)} must match workspace version ${version}`);
    }
  }
  if (Object.keys(dependencies).length !== PLATFORM_PACKAGES.length) {
    throw new Error(`${directory}: optionalDependencies must contain exactly the supported platform matrix`);
  }
  console.log(`${directory}: portable pack verified (${paths.size} files)`);
}

for (const target of PLATFORM_PACKAGES) {
  const directory = path.join("packages", "native", target.id);
  const packageManifest = manifest(directory);
  if (
    packageManifest.name !== nativePackageName(target.id) ||
    packageManifest.version !== version ||
    packageManifest.publishConfig?.access !== "public"
  ) {
    throw new Error(`${directory}: name/version diverges from release matrix`);
  }
  if (JSON.stringify(packageManifest.os) !== JSON.stringify([target.os]) || JSON.stringify(packageManifest.cpu) !== JSON.stringify([target.cpu])) {
    throw new Error(`${directory}: os/cpu do not match ${target.id}`);
  }
  if (target.libc && JSON.stringify(packageManifest.libc) !== JSON.stringify([target.libc])) {
    throw new Error(`${directory}: libc must declare ${target.libc}`);
  }
  if (packageManifest.main !== addonName(target.id)) {
    throw new Error(`${directory}: main must load ${addonName(target.id)}`);
  }
  if (JSON.stringify(packageManifest.files) !== JSON.stringify([executableName(target.id), addonName(target.id), "LICENSE-APACHE", "LICENSE-MIT", heroImage])) {
    throw new Error(`${directory}: files must contain exactly its executable, N-API addon, both license texts, and ${heroImage}`);
  }
  const paths = pack(directory);
  requireLicenses(directory, paths);
  const required = [addonName(target.id), executableName(target.id)];
  if (requireAllPlatforms) {
    for (const file of required) {
      const artifact = path.join(directory, file);
      if (!existsSync(artifact) || statSync(artifact).size === 0 || !paths.has(file)) {
        throw new Error(`${directory}: ${file} is missing, empty, or excluded from npm pack`);
      }
    }
  }
  console.log(`${directory}: ${requireAllPlatforms ? "publishable artifacts verified" : "manifest verified"}`);
}

const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--no-deps", "--format-version=1"], { encoding: "utf8" }));
for (const packageName of ["freellama-core", "freellama-cli"]) {
  const packageInfo = metadata.packages.find((entry) => entry.name === packageName);
  if (!packageInfo || !Array.isArray(packageInfo.publish) || packageInfo.publish.length !== 0) {
    throw new Error(`${packageName}: must set publish = false; npm packages bundle the internal Rust artifacts.`);
  }
  if (packageInfo.version !== version) {
    throw new Error(`${packageName}: version ${packageInfo.version} must match workspace version ${version}`);
  }
}

console.log(requireAllPlatforms ? "all release package contents verified; run release:verify:registry before publication" : "package manifests verified; run release:assemble then release:verify:publish before npm publish");
