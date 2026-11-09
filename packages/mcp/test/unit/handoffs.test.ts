import { WORKFLOW_NAMES } from "../../src/local-prompts.js";
// saveAs handoffs: the file format, the readable root that lets a later task read one, and sweep
// results saved whole. MCP prompts: each returns the exact call. No serve or Ollama.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, symlinkSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  const { mkdtempSync } = process.getBuiltinModule("node:fs") as typeof import("node:fs");
  const { tmpdir } = process.getBuiltinModule("node:os") as typeof import("node:os");
  const { join } = process.getBuiltinModule("node:path") as typeof import("node:path");
  const home = mkdtempSync(join(tmpdir(), "freellama-handoff-home-"));
  process.env.FREELLAMA_HOME = home;
  process.env.FREELLAMA_SWEEP_DIR = join(home, "sweeps");
});
vi.mock("../../src/serve-client.js", () => ({ runTaskRequest: vi.fn(), getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn() }));

import { checkOutputPath, HANDOFF_DIR, OUTPUT_TMP_DIR, pruneOutputDir, summaryOf, writeDefaultOutput, writeHandoff, writeOutputFile } from "../../src/handoffs.js";
import { applyTaskFiles, fileRoots } from "../../src/task-files.js";
import { registerRunTaskTool, withJob } from "../../src/tools/run-task.js";
import { registerDelegateResearchTool } from "../../src/tools/delegate-research.js";
import { runTaskRequest } from "../../src/serve-client.js";
import { registerSweepTool } from "../../src/tools/sweep.js";
import { defaultFlags, SweepRunner, type Executor } from "../../src/sweep/runner.js";
import { SweepStore, SWEEP_ROOT } from "../../src/sweep/store.js";
import { BUILTIN_PROMPTS, perFile, registerBuiltinPrompts, workflowPrompt } from "../../src/builtin-prompts.js";

describe("handoff files", () => {
  it("writes a summary and the whole result, and refuses a name that is not a short slug", () => {
    const dir = mkdtempSync(path.join(tmpdir(), "freellama-handoffs-"));
    const file = writeHandoff("lint-notes", { source: "From run_task", summary: "two errors", result: "a\nb" }, dir);
    expect(file).toBe(path.join(dir, "lint-notes.md"));
    expect(readFileSync(file, "utf8")).toMatch(/^# lint-notes\n\nFrom run_task, \d{4}-.*\n\n## Summary\n\ntwo errors\n\n## Result\n\na\nb\n$/s);
    expect(() => writeHandoff("../escape", { source: "", summary: "", result: "" }, dir)).toThrow(/a-z, 0-9 and -/);
    expect(summaryOf("\n1\n2\n\n3\n4\n5\n6")).toBe("1\n2\n3\n4\n5");
  });

  it("is readable by a later task through files, from the FreeLlama home", () => {
    expect(HANDOFF_DIR).toBe(path.join(process.env.FREELLAMA_HOME!, "handoffs"));
    const home = mkdtempSync(path.join(tmpdir(), "freellama-roots-home-"));
    expect(fileRoots({ FREELLAMA_HOME: home }, "/")).toContain(path.join(realpathSync(home), "handoffs"));
    expect(fileRoots({}, "/", home)).toContain(path.join(realpathSync(home), ".octocode", "freellama", "handoffs"));
    const file = writeHandoff("for-next", { source: "From run_task", summary: "s", result: "r" }, path.join(home, "handoffs"));
    const { payload } = applyTaskFiles({ task: "completion", prompt: "Use this:" }, [file], fileRoots({ FREELLAMA_HOME: home }, "/"));
    expect(payload.prompt).toContain("## Result\n\nr");
  });

  it("saves the answer text without its model label, and leaves errors alone", () => {
    const dir = mkdtempSync(path.join(tmpdir(), "freellama-handoffs-"));
    const result = { content: [{ type: "text", text: "[m:1 · cold load] - one\n- two" }], structuredContent: { route: { selected_model: "m:1" } } };
    const saved = withJob(result, undefined, "notes", dir);
    expect(saved.structuredContent).toMatchObject({ handoff_file: path.join(dir, "notes.md") });
    expect(readFileSync(path.join(dir, "notes.md"), "utf8")).toContain("From run_task, model m:1,");
    expect(readFileSync(path.join(dir, "notes.md"), "utf8")).toContain("## Result\n\n- one\n- two\n");
    const failed = { isError: true, content: [{ type: "text", text: "refused" }] };
    expect(withJob(failed, { job: "ocr", model: "x", applied: [] }, "notes", dir)).toBe(failed);
  });

  it("saves every chunk a sweep results filter matches, under the sweep's counts", async () => {
    expect(SWEEP_ROOT).toBe(process.env.FREELLAMA_SWEEP_DIR);
    const root = mkdtempSync(path.join(tmpdir(), "freellama-sweep-"));
    mkdirSync(path.join(root, "src"));
    writeFileSync(path.join(root, "src", "a.ts"), "const a = 1;\n// TODO leak\n");
    writeFileSync(path.join(root, "src", "b.ts"), "const b = 2;\n");
    const questions = { bug: { type: "noul" as const, instructions: "Is there a bug?" } };
    const model: Executor = async (request) => ({ response: { answers: { bug: { type: "noul", noul: String(request.state).includes("leak") ? 0.93 : 0.05 } } } });
    const runner = new SweepRunner(new SweepStore(SWEEP_ROOT), model, async () => undefined);
    const started = await runner.start({ model: "decider", questions, flags: defaultFlags(questions), unsureMargin: 0.15, priority: "background",
      maxParallelism: 1, chunkBytes: 3000, source: { kind: "files", files: [path.join(root, "src", "*.ts")] } });
    await runner.waitFor(started.id, 5_000);
    const handlers = new Map<string, (args: any, extra: any) => Promise<any>>();
    registerSweepTool({ registerTool: (name: string, _config: unknown, handler: any) => handlers.set(name, handler) } as unknown as McpServer);
    const result = await handlers.get("sweep")!({ action: "results", sweepId: started.id, filter: "flagged", saveAs: "leaks" }, { signal: new AbortController().signal });
    expect(result.isError).toBeFalsy();
    const file = result.structuredContent.handoff_file;
    expect(file).toBe(path.join(HANDOFF_DIR, "leaks.md"));
    const body = readFileSync(file, "utf8");
    expect(body).toMatch(/## Summary\n\nsweep .* completed: 2\/2 chunks, 1 flagged, 0 unsure, 1 clear, 0 failed/);
    expect(body).toMatch(/## Result\n\n- .*a\.ts:L1-L\d+: flagged \(bug 0\.93\)\n$/);
    expect((await handlers.get("sweep")!({ action: "status", sweepId: started.id, saveAs: "x" }, {})).content[0].text).toMatch(/apply to action "results" only/);
  });
});

describe("outputPath files", () => {
  const scratch = () => realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-output-")));
  const content = { source: "From run_task", summary: "s", result: "the answer" };

  it("writes a Markdown file, creating missing directories, and replaces any existing regular file", () => {
    const dir = scratch();
    const target = path.join(dir, "a", "b", "out.md");
    expect(writeOutputFile(target, content)).toBe(target);
    expect(readFileSync(target, "utf8")).toMatch(/^# out\.md\n\nFrom run_task, \d{4}-.*## Result\n\nthe answer\n$/s);
    expect(writeOutputFile(target, { ...content, result: "newer" })).toBe(target);
    expect(readFileSync(target, "utf8")).toContain("## Result\n\nnewer\n");
    writeFileSync(path.join(dir, "mine.md"), "# my notes\n");
    writeOutputFile(path.join(dir, "mine.md"), content);
    expect(readFileSync(path.join(dir, "mine.md"), "utf8")).toContain("## Result\n\nthe answer\n");
  });

  it("refuses paths outside the writable roots, inside .git, through a symlink escape, and directories", () => {
    const dir = scratch();
    const outside = scratch();
    expect(() => checkOutputPath(path.join(outside, "x.md"), [dir])).toThrow(/outside the writable roots/);
    expect(() => checkOutputPath(path.join(dir, "..", "x.md"), [dir])).toThrow(/outside the writable roots/);
    expect(() => checkOutputPath(path.join(dir, ".git", "hooks", "x.md"), [dir])).toThrow(/inside a \.git directory/);
    symlinkSync(outside, path.join(dir, "link"));
    expect(() => checkOutputPath(path.join(dir, "link", "x.md"), [dir])).toThrow(/outside the writable roots/);
    writeFileSync(path.join(outside, "secret.md"), "x");
    symlinkSync(path.join(outside, "secret.md"), path.join(dir, "file-link.md"));
    expect(() => checkOutputPath(path.join(dir, "file-link.md"), [dir])).toThrow(/outside the writable roots/);
    mkdirSync(path.join(dir, "folder"));
    expect(() => checkOutputPath(path.join(dir, "folder"), [dir])).toThrow(/not a regular file/);
    expect(checkOutputPath(path.join(dir, "new", "x.md"), [dir])).toBe(path.join(dir, "new", "x.md"));
  });

  it("writes default output files that never collide, under a directory a later task can read", () => {
    expect(OUTPUT_TMP_DIR).toBe(path.join(process.env.FREELLAMA_HOME!, "tmp"));
    const home = mkdtempSync(path.join(tmpdir(), "freellama-roots-home-"));
    expect(fileRoots({ FREELLAMA_HOME: home }, "/")).toContain(path.join(realpathSync(home), "tmp"));
    const dir = path.join(home, "tmp");
    const now = new Date("2026-11-09T10:11:12.345Z");
    const first = writeDefaultOutput(content, dir, now, 7);
    const second = writeDefaultOutput(content, dir, now, 7);
    expect(first).not.toBe(second);
    expect(path.basename(first)).toMatch(/^2026-11-09T10-11-12-345Z-\d+-\d+\.md$/);
    const { payload } = applyTaskFiles({ task: "completion", prompt: "Use this:" }, [first], fileRoots({ FREELLAMA_HOME: home }, "/"));
    expect(payload.prompt).toContain("## Result\n\nthe answer");
  });

  it("prunes default output files older than the TTL on the next write, and skips what it cannot remove", () => {
    const dir = path.join(mkdtempSync(path.join(tmpdir(), "freellama-prune-")), "tmp");
    mkdirSync(dir);
    const now = new Date("2026-11-09T00:00:00.000Z");
    const age = (name: string, days: number, body = "x") => {
      const file = path.join(dir, name);
      writeFileSync(file, body);
      const when = new Date(now.getTime() - days * 86_400_000);
      utimesSync(file, when, when);
    };
    age("old.md", 8);
    age("recent.md", 6);
    age("old.txt", 30);
    mkdirSync(path.join(dir, "folder.md"));
    const fresh = writeDefaultOutput(content, dir, now, 7);
    expect(readdirSync(dir).sort()).toEqual(["folder.md", "old.txt", path.basename(fresh), "recent.md"].sort());
    // TTL 0 keeps everything; a missing directory is not an error.
    age("older.md", 400);
    writeDefaultOutput(content, dir, now, 0);
    expect(existsSync(path.join(dir, "older.md"))).toBe(true);
    expect(() => pruneOutputDir(path.join(dir, "missing"), now.getTime(), 7)).not.toThrow();
  });

  it("is refused by run_task and delegate_research before any model work, and with preview or defer", async () => {
    const handlers = new Map<string, (args: any, extra: any) => Promise<any>>();
    const server = { registerTool: (name: string, _config: unknown, handler: any) => handlers.set(name, handler) } as unknown as McpServer;
    registerRunTaskTool(server);
    registerDelegateResearchTool(server);
    const extra = { signal: new AbortController().signal };
    const dir = scratch();
    const bad = path.join(path.parse(dir).root, "freellama-not-a-root", "x.md");
    const run = handlers.get("run_task")!;
    expect((await run({ prompt: "p", outputPath: bad }, extra)).content[0].text).toMatch(/outside the writable roots/);
    expect((await run({ prompt: "p", outputPath: path.join(dir, ".git", "x.md") }, extra)).content[0].text).toMatch(/inside a \.git directory/);
    expect((await run({ outputPath: bad, tasks: [{ id: "a", independent: true, task: { prompt: "p" } }] }, extra)).content[0].text).toMatch(/outside the writable roots/);
    for (const flag of [{ preview: true }, { defer: true }]) {
      expect((await run({ prompt: "p", outputPath: path.join(dir, "ok.md"), ...flag }, extra)).content[0].text).toMatch(/not valid with `preview` or `defer`/);
    }
    expect((await handlers.get("delegate_research")!({ question: "q", workspacePath: dir, outputPath: bad }, extra)).content[0].text).toMatch(/outside the writable roots/);
    expect(runTaskRequest).not.toHaveBeenCalled();
  });
});

describe("MCP prompts", () => {
  const text = (result: { messages: Array<{ content: { text: string } }> }) => result.messages[0]!.content.text;

  it("registers one prompt per workflow, the non-prompt tools and the freellama guide", () => {
    const names: string[] = [];
    registerBuiltinPrompts({ registerPrompt: (name: string) => names.push(name) } as unknown as McpServer);
    expect(names).toEqual([...WORKFLOW_NAMES, "classify", "find-code", "duplicates", "research", "github-research", "sweep", "freellama"]);
  });

  it("returns the exact call, one task per file when several files match", () => {
    const root = mkdtempSync(path.join(tmpdir(), "freellama-prompt-"));
    writeFileSync(path.join(root, "a.md"), "a");
    writeFileSync(path.join(root, "b.md"), "b");
    const many = perFile(path.join(root, "*.md"), { job: "classify" });
    expect(many.args).toEqual({ job: "classify", tasks: [
      { id: path.join(root, "a.md"), independent: true, task: { files: [path.join(root, "a.md")] } },
      { id: path.join(root, "b.md"), independent: true, task: { files: [path.join(root, "b.md")] } },
    ] });
    expect(text(workflowPrompt("read-table", "shots/x.png"))).toContain('run_task {"workflow":"read-table","files":["shots/x.png"]}');
    expect(text(workflowPrompt("summarize-log", "logs/app.log", "why did checkout fail"))).toContain('run_task {"workflow":"summarize-log","files":["logs/app.log"],"prompt":"why did checkout fail"}');
    expect(text(BUILTIN_PROMPTS.classify.build({ files: "issue.md", question: "What kind of issue is this?", labels: "bug, feature" })))
      .toContain('"criteria":{"bug":"bug","feature":"feature","other":"None of these fits"}');
    expect(text(BUILTIN_PROMPTS["find-code"].build({ question: "where are ports parsed", files: "src/**/*.ts" })))
      .toContain('run_task {"job":"find-code","files":["src/**/*.ts"],"nearest":{"query":"where are ports parsed","top":5}}');
    expect(text(workflowPrompt("fix-bug", "src/a.ts", "TypeError: x is undefined"))).toMatch(/run_task \{"workflow":"fix-bug".*SEARCH text occurs exactly once.*check: lines/s);
    expect(text(workflowPrompt("review-code", "src/a.ts"))).not.toMatch(/SEARCH text occurs/);
    expect(text(BUILTIN_PROMPTS["github-research"].build({ question: "Where is X?", repo: "ollama/ollama" })))
      .toContain('delegate_research {"question":"In ollama/ollama: Where is X?","adapter":"github","model":"bonsai-2:9b"}');
    expect(text(BUILTIN_PROMPTS.sweep.build({ files: "src/**/*.ts", question: "Does this code swallow an error?" })))
      .toContain('"model":"strands-decider:2b-nvfp4","questions":{"flag":{"type":"noul","instructions":"Does this code swallow an error?"}}');
  });
});
