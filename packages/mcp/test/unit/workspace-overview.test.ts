import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { workspaceOverview } from "../../src/research/workspace-overview.js";

describe("research workspace overview", () => {
  it("names the top level and the dominant file types, skipping vendored trees", () => {
    const root = mkdtempSync(path.join(tmpdir(), "freellama-overview-"));
    mkdirSync(path.join(root, "src", "sweep"), { recursive: true });
    mkdirSync(path.join(root, "node_modules", "x"), { recursive: true });
    for (const name of ["a.ts", "b.ts", "sweep/c.ts"]) writeFileSync(path.join(root, "src", name), "x");
    writeFileSync(path.join(root, "README.md"), "x");
    for (let index = 0; index < 50; index++) writeFileSync(path.join(root, "node_modules", "x", `${index}.js`), "x");
    const overview = workspaceOverview(root);
    expect(overview).toContain("top level: src/, README.md");
    expect(overview).toMatch(/4 files; most common types: \.ts 3, \.md 1/);
    expect(overview).not.toContain(".js");
    expect(overview).toMatch(/grep -rn/);
  });

  it("is empty for a missing workspace", () => {
    expect(workspaceOverview(path.join(tmpdir(), "freellama-missing-overview"))).toBe("");
  });
});
