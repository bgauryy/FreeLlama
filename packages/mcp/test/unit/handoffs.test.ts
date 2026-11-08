import { WORKFLOW_NAMES } from "../../src/local-prompts.js";
// saveAs handoffs: the file format, the readable root that lets a later task read one, and sweep
// results saved whole. MCP prompts: each returns the exact call. No serve or Ollama.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
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

import { HANDOFF_DIR, summaryOf, writeHandoff } from "../../src/handoffs.js";
import { applyTaskFiles, fileRoots } from "../../src/task-files.js";
import { withJob } from "../../src/tools/run-task.js";
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
