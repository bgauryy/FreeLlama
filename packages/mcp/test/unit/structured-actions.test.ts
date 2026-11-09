import { mkdirSync, mkdtempSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { parseAction } from "../../src/research/bash-agent.js";
import { ReadOnlyShell } from "../../src/research/sandbox.js";
import { confined, listEntries, looksLikeRegex, pathCandidates, readLines, resolveNamedPath, searchArgv } from "../../src/research/structured-actions.js";
import { workspaceOverview } from "../../src/research/workspace-overview.js";
import { BASH_ADAPTER } from "../../src/research/bash-agent.js";

const root = realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-structured-")));
const outside = realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-outside-")));
mkdirSync(path.join(root, "src", "sweep"), { recursive: true });
mkdirSync(path.join(root, "node_modules", "dep"), { recursive: true });
writeFileSync(path.join(root, "src", "sweep", "sources.ts"), "const a = 1;\n// robots.txt rules\nget(`${origin}/robots.txt`);\n");
writeFileSync(path.join(root, "node_modules", "dep", "x.ts"), "robots.txt in a dependency\n");
writeFileSync(path.join(outside, "secret.txt"), "robots.txt secret\n");
symlinkSync(outside, path.join(root, "escape"));

describe("structured research actions", () => {
  it("parses search, read and list, and rejects bad shapes with a useful message", () => {
    expect(parseAction('{"action":"search","pattern":"robots.txt","glob":"*.ts"}')).toMatchObject({ action: "search" });
    expect(parseAction('{"action":"read","path":"src/a.ts","start":3}')).toMatchObject({ action: "read" });
    expect(parseAction('{"action":"list"}')).toMatchObject({ action: "list" });
    expect(() => parseAction('{"action":"search","pattern":""}')).toThrow(/non-empty string pattern/);
    expect(() => parseAction('{"action":"read","path":"a","start":0}')).toThrow(/positive integer/);
    expect(() => parseAction('{"action":"grep","pattern":"x"}')).toThrow(/use search, read, list, shell, page or finish/);
  });

  it("confines every path to the workspace, through symlinks too", () => {
    expect(confined(root, "src/sweep")).toBe("src/sweep");
    expect(confined(root, undefined)).toBe(".");
    expect(() => confined(root, "../")).toThrow(/outside the workspace/);
    expect(() => confined(root, "/etc")).toThrow(/outside the workspace/);
    expect(() => confined(root, "escape/secret.txt")).toThrow(/outside the workspace/);
  });

  it("builds a literal, vendored-tree-free search by default", () => {
    const argv = searchArgv(root, { pattern: "${origin}(" }, true);
    expect(argv).toContain("-F");
    expect(argv).toEqual(expect.arrayContaining(["-g", "!node_modules/", "-e", "${origin}(", "--", "."]));
    expect(searchArgv(root, { pattern: "a.b", regex: true, ignoreCase: true, glob: "*.ts" }, false))
      .toEqual(expect.arrayContaining(["-E", "-i", "--include=*.ts", "--exclude-dir=node_modules"]));
  });

  it("reads numbered line ranges and lists without vendored trees", () => {
    expect(readLines(root, { path: "src/sweep/sources.ts", start: 2, end: 3 })).toBe("src/sweep/sources.ts:2-3\n2  // robots.txt rules\n3  get(`${origin}/robots.txt`);");
    expect(() => readLines(root, { path: "src/nope.ts" })).toThrow(/no such file: src\/nope\.ts does not exist and no path ends with it; search or list/);
    const listing = listEntries(root, undefined);
    expect(listing).toContain("src/sweep/");
    expect(listing).not.toContain("node_modules");
  });

  describe("through the read-only shell", () => {
    let shell: ReadOnlyShell;
    beforeAll(() => { shell = new ReadOnlyShell(root).enter(); });
    afterAll(() => shell.close());

    it("searches with ripgrep or grep without a shell, finding source hits only", async () => {
      const output = await shell.runTool(searchArgv(root, { pattern: "${origin}/robots.txt" }, shell.hasTool("rg")), 10);
      expect(output).toMatch(/src\/sweep\/sources\.ts:3:/);
      expect(output).not.toContain("node_modules");
      expect(output).not.toContain("secret");
      expect(await shell.runTool(searchArgv(root, { pattern: "no-such-text-anywhere" }, shell.hasTool("rg")), 10)).toBe("(no matches)");
    });

    it("refuses a tool that is not read-only", async () => {
      await expect(shell.runTool(["rm", "-rf", "."], 5)).rejects.toThrow(/not available/);
    });
  });

  it("resolves a path written relative to a package or taken from the question", () => {
    expect(pathCandidates(root, "sweep/sources.ts")).toEqual(["src/sweep/sources.ts"]);
    expect(resolveNamedPath(root, "sweep/sources.ts", "file")).toEqual({ path: "src/sweep/sources.ts", note: "(sweep/sources.ts does not exist; using src/sweep/sources.ts)" });
    expect(resolveNamedPath(root, "nowhere/x.ts", "file")).toMatchObject({ path: undefined, note: expect.stringMatching(/no path ends with it/) });
    expect(readLines(root, { path: "sweep/sources.ts", end: 1 })).toMatch(/^\(sweep\/sources\.ts does not exist; using src\/sweep\/sources\.ts\)\nsrc\/sweep\/sources\.ts:1-1/);
  });

  it("recognizes regex syntax a model sent as a literal, and leaves code text alone", () => {
    expect(looksLikeRegex("memory|heap")).toBe(true);
    expect(looksLikeRegex("fn\\s+main")).toBe(true);
    expect(looksLikeRegex("foo(")).toBe(false);
    expect(looksLikeRegex("a.b")).toBe(false);
  });

  it("names source folders in the overview", () => {
    expect(workspaceOverview(root)).toMatch(/Source folders: src\/ \(1\)/);
  });

  it("searches a missing path across the workspace and retries an alternation as a regex", async () => {
    const session = await BASH_ADAPTER.open!(root);
    try {
      const run = (args: Record<string, unknown>) => session.toolCall({ action: "search", ...args }, root,
        { tool_timeout_seconds: 10 } as never).run();
      const out = await run({ pattern: "robots.txt rules|nothing", path: "lib" });
      expect(out).toMatch(/lib does not exist and no path ends with it; searched the whole workspace/);
      expect(out).toMatch(/no literal match; searched as a regular expression/);
      expect(out).toMatch(/src\/sweep\/sources\.ts:2:/);
      expect(await run({ pattern: "robots", path: "sweep" })).toMatch(/using src\/sweep\)\n.*src\/sweep\/sources\.ts/s);
    } finally {
      session.close?.();
    }
  });
});

