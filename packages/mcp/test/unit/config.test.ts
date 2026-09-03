import { existsSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  assertAllowedWorkspace,
  parseAllowedResearchRoots,
  REPO_ROOT,
  RESEARCH_ADAPTERS,
} from "../../src/config.js";

describe("REPO_ROOT", () => {
  it("anchors on the directory containing Cargo.toml", () => {
    expect(existsSync(path.join(REPO_ROOT, "Cargo.toml"))).toBe(true);
  });
});

describe("research adapters", () => {
  it("resolves both adapters to files that exist", () => {
    expect(existsSync(RESEARCH_ADAPTERS.bash)).toBe(true);
    expect(existsSync(RESEARCH_ADAPTERS.octocode)).toBe(true);
  });
});

describe("research root parsing", () => {
  it("preserves Windows drive letters when using the Windows path-list separator", () => {
    expect(parseAllowedResearchRoots("C:\\code;D:\\work", ";")).toEqual([
      path.resolve("C:\\code"),
      path.resolve("D:\\work"),
    ]);
  });
});

describe("assertAllowedWorkspace", () => {
  it("accepts the repo root and directories inside it", async () => {
    await expect(assertAllowedWorkspace(REPO_ROOT)).resolves.toBeTruthy();
    await expect(assertAllowedWorkspace(path.join(REPO_ROOT, "packages"))).resolves.toBeTruthy();
  });

  it("rejects a path outside the allowed roots", async () => {
    await expect(assertAllowedWorkspace("/etc")).rejects.toThrow(/outside the allowed research roots/);
  });

  it("rejects a prefix-collision sibling (repo-root + suffix) — boundary is path-segment aware", async () => {
    // `${REPO_ROOT}-evil` startsWith REPO_ROOT but is NOT inside it; it also doesn't exist, so
    // either rejection message is correct — what must never happen is acceptance.
    await expect(assertAllowedWorkspace(`${REPO_ROOT}-evil`)).rejects.toThrow();
  });

  it("rejects a path that does not exist", async () => {
    await expect(assertAllowedWorkspace(path.join(REPO_ROOT, "no-such-dir-xyz"))).rejects.toThrow(
      /does not exist/,
    );
  });
});
