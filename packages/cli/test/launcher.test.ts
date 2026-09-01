// The npm wrapper around the compiled Rust binary. Two contracts worth pinning:
// the no-binary error must name what it looked for (not a bare ENOENT), and with a binary
// present the launcher must hand through args, stdio, and the exit code.
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const PKG = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const LAUNCHER = path.join(PKG, "bin", "freellama.mjs");
const RELEASE_BINARY = path.join(PKG, "..", "..", "target", "release", "freellama");

describe("freellama launcher", () => {
  it("fails with a named, actionable error when no binary exists for the platform", () => {
    // Copy the launcher somewhere with no vendor/ and no ../../target/release next to it.
    const scratch = mkdtempSync(path.join(os.tmpdir(), "freellama-launcher-"));
    mkdirSync(path.join(scratch, "bin"));
    const orphan = path.join(scratch, "bin", "freellama.mjs");
    copyFileSync(LAUNCHER, orphan);

    const result = spawnSync("node", [orphan, "--help"], { encoding: "utf8" });
    expect(result.status).toBe(1);
    expect(result.stderr).toMatch(/no binary for/);
    expect(result.stderr).toMatch(/cargo build --release/);
  });

  it.runIf(existsSync(RELEASE_BINARY))("forwards args and the exit code to the real binary", () => {
    const help = spawnSync("node", [LAUNCHER, "--help"], { encoding: "utf8" });
    expect(help.status).toBe(0);
    expect(help.stdout.length).toBeGreaterThan(0);

    const bogus = spawnSync("node", [LAUNCHER, "--definitely-not-a-flag"], { encoding: "utf8" });
    expect(bogus.status).not.toBe(0);
  });
});
