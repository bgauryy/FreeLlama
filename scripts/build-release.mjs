#!/usr/bin/env node
// Builds the CLI executable and N-API addon for every release target (or the ids given as
// arguments) into release-artifacts/<id>/, the input of scripts/assemble-release.mjs.
// darwin targets build natively; linux uses cargo-zigbuild and win32 uses cargo-xwin.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { PLATFORM_PACKAGES, addonName, executableName } from "./release-platforms.mjs";
import { requireEmbeddedVersion } from "./release-version.mjs";

const root = path.resolve(import.meta.dirname, "..");
const version = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8")).version;
const requested = process.argv.slice(2);
const unknown = requested.filter((id) => !PLATFORM_PACKAGES.some((target) => target.id === id));
if (unknown.length) throw new Error(`unknown platform id(s): ${unknown.join(", ")}`);
const targets = requested.length ? PLATFORM_PACKAGES.filter(({ id }) => requested.includes(id)) : PLATFORM_PACKAGES;

const run = (command, args) => execFileSync(command, args, { cwd: root, stdio: "inherit" });
const cargoCommand = { darwin: ["build"], linux: ["zigbuild"], win32: ["xwin", "build"] };

for (const target of targets) {
  const out = path.join(root, "release-artifacts", target.id);
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  console.log(`\n=== ${target.id} (${target.rustTarget})`);

  run("yarn", [
    "napi", "build", "--platform", "--release", "--target", target.rustTarget,
    ...(target.os === "darwin" ? [] : ["--cross-compile"]),
    "--manifest-path", "packages/rust-core/Cargo.toml", "--features", "napi",
    "--no-js", "--no-dts-header", "-o", out,
  ]);
  run("cargo", [...cargoCommand[target.os], "--release", "--target", target.rustTarget, "-p", "freellama-cli"]);
  copyFileSync(
    path.join(root, "target", target.rustTarget, "release", executableName(target.id)),
    path.join(out, executableName(target.id)),
  );

  rmSync(path.join(out, "index.d.ts"), { force: true });

  for (const file of [addonName(target.id), executableName(target.id)]) {
    requireEmbeddedVersion(path.join(out, file), version);
  }
  if (target.os === process.platform && target.cpu === process.arch && target.libc !== "musl") {
    const reported = [
      createRequire(import.meta.url)(path.join(out, addonName(target.id))).version(),
      execFileSync(path.join(out, executableName(target.id)), ["--version"], { encoding: "utf8" }).trim(),
    ];
    if (reported[0] !== version || reported[1] !== `freellama ${version}`) {
      throw new Error(`${target.id}: host artifacts report ${reported.join(" / ")}, expected ${version}`);
    }
  }
}

console.log(`\nbuilt ${targets.length} target(s) at ${version} into release-artifacts/; next: yarn release:assemble`);
