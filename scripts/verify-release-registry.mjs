#!/usr/bin/env node
// Read-only release preflight: every package version must be absent before any publish.
import { readFileSync } from "node:fs";
import path from "node:path";
import { PLATFORM_PACKAGES, nativePackageName } from "./release-platforms.mjs";

const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const manifest = (directory) => JSON.parse(readFileSync(path.join(directory, "package.json"), "utf8"));

async function main() {
  const root = manifest(".");
  const version = root.version;
  if (typeof version !== "string" || !version.trim()) throw new Error("workspace version must be a nonempty string");
  const releaseTag = process.env.FREELLAMA_RELEASE_TAG;
  if (releaseTag && releaseTag !== `v${version}`) {
    throw new Error(`release tag ${releaseTag} must match workspace version v${version}`);
  }
  const packages = [
    ["packages/cli", "@octocodeai/freellama"],
    ["packages/mcp", "@octocodeai/freellama-mcp-server"],
    ...PLATFORM_PACKAGES.map(({ id }) => [`packages/native/${id}`, nativePackageName(id)]),
  ];
  // Validate the entire local matrix before sending even the first registry request.
  for (const [directory, expectedName] of packages) {
    const info = manifest(directory);
    if (info.name !== expectedName) throw new Error(`${directory}: package name must be ${expectedName}`);
    if (info.version !== version) {
      throw new Error(`${directory}: version ${info.version} must match workspace version ${version}`);
    }
  }
  const registry = new URL(process.env.FREELLAMA_NPM_REGISTRY ?? "https://registry.npmjs.org/");
  if (!["http:", "https:"].includes(registry.protocol) || registry.username || registry.password || registry.search || registry.hash) {
    throw new Error("npm registry must be an HTTP(S) URL without credentials, query, or fragment");
  }
  if (!registry.pathname.endsWith("/")) registry.pathname += "/";
  const timeoutMs = Number(process.env.FREELLAMA_NPM_REGISTRY_TIMEOUT_MS ?? 15_000);
  if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60_000) {
    throw new Error("FREELLAMA_NPM_REGISTRY_TIMEOUT_MS must be an integer from 1 through 60000");
  }

  const checks = await Promise.all(packages.map(async ([, name]) => {
    const identity = `${name}@${version}`;
    const signal = AbortSignal.timeout(timeoutMs);
    let response;
    try {
      response = await fetch(new URL(encodeURIComponent(name), registry), {
        method: "GET",
        headers: { accept: "application/json" },
        signal,
      });
    } catch (error) {
      return `${identity}: registry request failed (${error.message})`;
    }
    if (response.status === 404) {
      await response.body?.cancel();
      return null;
    }
    if (!response.ok) {
      await response.body?.cancel();
      return `${identity}: registry returned HTTP ${response.status}; absence was not established`;
    }
    let metadata;
    try {
      metadata = await response.json();
    } catch (error) {
      return signal.aborted
        ? `${identity}: registry request failed (${error.message})`
        : `${identity}: invalid registry metadata (response is not JSON)`;
    }
    if (!object(metadata) || metadata.name !== name || !object(metadata.versions)
        || Object.entries(metadata.versions).some(([publishedVersion, entry]) => !object(entry) || entry.version !== publishedVersion)) {
      return `${identity}: invalid registry metadata (matching name and versions object required)`;
    }
    if (Object.hasOwn(metadata.versions, version)) return `${identity}: already published`;
    return null;
  }));
  const failures = checks.filter(Boolean);
  if (failures.length) throw new Error(`release registry preflight failed:\n${failures.join("\n")}`);
  console.log(`registry preflight passed: ${packages.length} package versions absent for ${version}`);
}

try {
  await main();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
