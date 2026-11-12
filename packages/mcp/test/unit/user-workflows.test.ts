import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterAll, describe, expect, it, vi } from "vitest";
import { loadWorkflowFiles, parseWorkflowFile } from "../../src/workflow-files.js";

const JOBS = ["summarize", "coding", "ocr"];
const RESERVED = [...JOBS, "auto"];
const file = (front: string, body = "Do the thing.") => `---\n${front}\n---\n${body}\n`;

describe("parsing a user workflow file", () => {
  it("reads the front matter and takes the body as the local model's instructions", () => {
    expect(parseWorkflowFile("meeting-notes", file('job: summarize\ntitle: "Summarize a meeting: decisions, owners"\nthink: false\nmax_output: 400\nprefer: capable\nformat: json  # structured'), JOBS, RESERVED))
      .toEqual({ job: "summarize", title: "Summarize a meeting: decisions, owners", input: expect.any(String), system: "Do the thing.", think: false, maxOutput: 400, prefer: "capable", format: "json" });
  });

  it("parses the example in docs/PROMPTS.md exactly as documented", () => {
    const doc = readFileSync(new URL("../../../../docs/PROMPTS.md", import.meta.url), "utf8");
    const example = /## Your own workflows[\s\S]*?```markdown\n([\s\S]*?)```/.exec(doc)![1]!;
    expect(parseWorkflowFile("meeting-notes", example, ["summarize"], [])).toEqual({
      job: "summarize", title: "Summarize a meeting: decisions, owners, dates", input: "files: the transcript; prompt: what to focus on (optional)",
      think: false, maxOutput: 400, prefer: "capable", format: "json", system: expect.stringMatching(/^List each decision/),
    });
    expect(parseWorkflowFile("x-y", file('job: summarize\ntitle: "A # in a title"   # comment'), JOBS, RESERVED).title).toBe("A # in a title");
    // A Windows editor's byte-order mark is not part of the front matter.
    expect(parseWorkflowFile("x-y", `\uFEFF${file("job: summarize\ntitle: With BOM")}`, JOBS, RESERVED).title).toBe("With BOM");
  });

  it.each([
    ["Bad_Name", file("job: summarize\ntitle: t"), /file name/],
    ["coding", file("job: coding\ntitle: t"), /job or virtual model name/],
    ["auto", file("job: coding\ntitle: t"), /job or virtual model name/],
    ["no-front", "Just instructions.", /front matter/],
    ["no-job", file("title: t"), /needs "job:"/],
    ["bad-job", file("job: translate\ntitle: t"), /not one of/],
    ["no-title", file("job: summarize"), /needs "title:"/],
    ["unknown-key", file("job: summarize\ntitle: t\nmodel: x:1"), /unknown front matter key "model"/],
    ["twice", file("job: summarize\njob: coding\ntitle: t"), /appears twice/],
    ["bad-think", file("job: summarize\ntitle: t\nthink: maybe"), /think must be/],
    ["bad-cap", file("job: summarize\ntitle: t\nmax_output: 9"), /max_output/],
    ["bad-format", file("job: summarize\ntitle: t\nformat: yaml"), /format must be/],
    ["empty-body", file("job: summarize\ntitle: t", "  "), /instructions .* are empty/],
  ])("refuses %s with the reason", (name, text, reason) => {
    expect(() => parseWorkflowFile(name, text, JOBS, RESERVED)).toThrow(reason);
  });

  it("loads a directory, skipping invalid files with their reason and a missing directory quietly", () => {
    const dir = mkdtempSync(path.join(tmpdir(), "fl-flows-"));
    writeFileSync(path.join(dir, "good-one.md"), file("job: summarize\ntitle: Good"));
    writeFileSync(path.join(dir, "bad-one.md"), file("title: Bad"));
    writeFileSync(path.join(dir, "notes.txt"), "ignored");
    const loaded = loadWorkflowFiles(dir, JOBS, RESERVED);
    expect([...loaded.workflows.keys()]).toEqual(["good-one"]);
    expect(loaded.workflows.get("good-one")!.source).toEqual({ path: path.join(dir, "good-one.md"), digest: expect.stringMatching(/^[0-9a-f]{12}$/) });
    expect(loaded.errors).toEqual([{ path: path.join(dir, "bad-one.md"), error: expect.stringMatching(/needs "job:"/) }]);
    expect(loadWorkflowFiles(path.join(dir, "missing"), JOBS, RESERVED)).toEqual({ workflows: new Map(), errors: [] });
  });
});

describe("user workflows next to the built-ins", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "fl-user-flows-"));
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, "meeting-notes.md"), file("job: summarize\ntitle: Summarize a meeting: decisions and owners\nthink: false\nmax_output: 300", "List each decision and its owner."));
  writeFileSync(path.join(dir, "summarize-log.md"), file("job: summarize\ntitle: Our log summary", "Summarize the log our way."));
  writeFileSync(path.join(dir, "broken.md"), file("job: nope\ntitle: x"));
  // A decision job takes no instructions: such a file would load and then be ignored.
  writeFileSync(path.join(dir, "my-triage.md"), file("job: classify\ntitle: x"));
  const previous = process.env.FREELLAMA_WORKFLOWS_DIR;
  afterAll(() => {
    if (previous === undefined) delete process.env.FREELLAMA_WORKFLOWS_DIR;
    else process.env.FREELLAMA_WORKFLOWS_DIR = previous;
    vi.resetModules();
  });

  async function fresh() {
    process.env.FREELLAMA_WORKFLOWS_DIR = dir;
    vi.resetModules();
    return {
      prompts: await import("../../src/local-prompts.js"),
      jobMode: await import("../../src/job-mode.js"),
      virtual: await import("../../src/agent-cli/virtual.js"),
    };
  }

  it("adds new names, replaces a built-in of the same name, and reports skipped files", async () => {
    const { prompts } = await fresh();
    expect(prompts.WORKFLOW_NAMES).toContain("meeting-notes");
    expect(prompts.WORKFLOWS["summarize-log"]!.system).toBe("Summarize the log our way.");
    expect(prompts.BUILTIN_WORKFLOWS["summarize-log"].system).not.toBe("Summarize the log our way.");
    expect(prompts.workflowSource("summarize-log")).toMatchObject({ kind: "user", overrides: true });
    expect(prompts.workflowSource("meeting-notes")).toMatchObject({ kind: "user", overrides: false });
    expect(prompts.workflowSource("fix-bug")).toEqual({ kind: "builtin" });
    expect(prompts.USER_WORKFLOW_ERRORS).toEqual([
      { path: path.join(dir, "broken.md"), error: expect.stringMatching(/not one of/) },
      { path: path.join(dir, "my-triage.md"), error: expect.stringMatching(/job "classify" is not one of ocr, image-recognition, coding, summarize/) },
    ]);
    // The jobs a file may name are exactly those whose task takes instructions.
    const prompted = prompts.RUN_TASK_JOBS.filter((job) => !["decision", "embedding"].includes(prompts.LOCAL_JOBS[job].task));
    expect([...prompts.PROMPTED_JOBS].sort()).toEqual([...prompted].sort());
    // A job's own default stays the eval-tested built-in.
    expect(prompts.LOCAL_JOBS.summarize.systemPrompt).toBe(prompts.BUILTIN_WORKFLOWS["summarize-text"].system);
  });

  it("runs a user workflow like a built-in, names its file in the profile, and lets caller fields win", async () => {
    const { jobMode } = await fresh();
    const applied = jobMode.applyJob("summarize", { prompt: "notes" }, "bonsai-2:9b", false, "meeting-notes");
    expect(applied.fields).toMatchObject({ systemPrompt: "List each decision and its owner.", think: false, options: expect.objectContaining({ num_predict: 300 }) });
    expect(applied.profile.workflow_source).toMatch(new RegExp(`meeting-notes\\.md \\(sha256 [0-9a-f]{12}\\)$`));
    const mine = jobMode.applyJob("summarize", { prompt: "notes", systemPrompt: "Mine." }, "bonsai-2:9b", false, "meeting-notes");
    expect(mine.fields.systemPrompt).toBe("Mine.");
  });

  it("serves a user workflow as a freellama/ virtual model", async () => {
    const { virtual } = await fresh();
    expect(virtual.virtualModel("freellama/meeting-notes")).toMatchObject({ workflow: "meeting-notes", job: "summarize" });
  });
});
