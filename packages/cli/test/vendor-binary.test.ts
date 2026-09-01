// The prepack step: copies target/release/freellama into vendor/<platform>-<arch>/ so `npm pack`
// ships something runnable. vendor/ is gitignored, so running the real script is side-effect-safe.
import { spawnSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const PKG = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SCRIPT = path.join(PKG, "scripts", "vendor-binary.mjs");
const RELEASE_BINARY = path.join(PKG, "..", "..", "target", "release", "freellama");

describe("vendor-binary (prepack)", () => {
  it.runIf(existsSync(RELEASE_BINARY))("stages the release binary under vendor/<platform>-<arch>/", () => {
    const result = spawnSync("node", [SCRIPT], { encoding: "utf8" });
    expect(result.status).toBe(0);

    const staged = path.join(PKG, "vendor", `${process.platform}-${process.arch}`, "freellama");
    expect(existsSync(staged)).toBe(true);
    expect(statSync(staged).size).toBe(statSync(RELEASE_BINARY).size);
  });

  it.runIf(!existsSync(RELEASE_BINARY))("fails loudly when no release binary has been built", () => {
    const result = spawnSync("node", [SCRIPT], { encoding: "utf8" });
    expect(result.status).toBe(1);
    expect(result.stderr).toMatch(/cargo build --release/);
  });
});
