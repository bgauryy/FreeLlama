// The npm wrapper around the compiled Rust binary. Two contracts worth pinning:
// the no-binary error must name what it looked for (not a bare ENOENT), and with a binary
// present the launcher must hand through args, stdio, and the exit code.
import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const PKG = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const LAUNCHER = path.join(PKG, "bin", "freellama.js");
const RELEASE_BINARY = path.join(PKG, "..", "..", "target", "release", "freellama");

describe("freellama launcher", () => {
  it("fails with a named, actionable error when no binary exists for the platform", () => {
    // Copy the launcher somewhere with no vendor/ and no ../../target/release next to it.
    const scratch = mkdtempSync(path.join(os.tmpdir(), "freellama-launcher-"));
    mkdirSync(path.join(scratch, "bin"));
    const orphan = path.join(scratch, "bin", "freellama.js");
    copyFileSync(LAUNCHER, orphan);

    try {
      // Exercise the launcher with this Node runtime; package managers may prepend shell shims.
      const result = spawnSync(process.execPath, [orphan, "--help"], { encoding: "utf8", timeout: 10_000 });
      expect(result.error).toBeUndefined();
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/no binary for/);
      expect(result.stderr).toMatch(/cargo build --release/);
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  });

  it.runIf(existsSync(RELEASE_BINARY))("forwards args and the exit code to the real binary", () => {
    const help = spawnSync(process.execPath, [LAUNCHER, "--help"], { encoding: "utf8", timeout: 10_000 });
    expect(help.error).toBeUndefined();
    expect(help.status).toBe(0);
    expect(help.stdout.length).toBeGreaterThan(0);

    const bogus = spawnSync(process.execPath, [LAUNCHER, "--definitely-not-a-flag"], { encoding: "utf8", timeout: 10_000 });
    expect(bogus.error).toBeUndefined();
    expect(bogus.status).not.toBe(0);
  });
});
