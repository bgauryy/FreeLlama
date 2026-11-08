// run_task {job}: the model pick from live state, the defaults local-prompts.ts supplies, the
// caller's fields winning, and job_profile/handoff_file on the result. Serve and Ollama are mocked.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { readFileSync } from "node:fs";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => {
  const { mkdtempSync } = process.getBuiltinModule("node:fs") as typeof import("node:fs");
  const { tmpdir } = process.getBuiltinModule("node:os") as typeof import("node:os");
  const { join } = process.getBuiltinModule("node:path") as typeof import("node:path");
  // Handoffs land here: config.ts reads FREELLAMA_HOME when it is first imported.
  process.env.FREELLAMA_HOME = mkdtempSync(join(tmpdir(), "freellama-job-home-"));
  return { run: (() => undefined) as any, live: (() => undefined) as any, handlers: new Map<string, (...args: any[]) => Promise<any>>() };
});
vi.mock("../../src/serve-client.js", () => ({ runTaskRequest: (...args: any[]) => fixture.run(...args), getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn() }));
vi.mock("../../src/model-library.js", () => ({ catalogLiveState: (...args: any[]) => fixture.live(...args) }));

import { applyJob, pickJobModel, familyMatch } from "../../src/job-mode.js";
import { MODEL_CATALOG } from "../../src/model-catalog.js";
import { LOCAL_JOBS, OCR_PROMPTS, RUN_TASK_JOBS } from "../../src/local-prompts.js";
import { registerRunTaskTool } from "../../src/tools/run-task.js";

const installed = (...names: string[]) => names.map((name) => ({ name }));
const GB = 1e9;

describe("local-prompts job table", () => {
  it("draws every job's picks from the catalog, in catalog order, and profiles every pick by family", () => {
    for (const job of RUN_TASK_JOBS) {
      const spec = LOCAL_JOBS[job];
      const catalogJob = MODEL_CATALOG.jobs.find((entry) => entry.job === spec.catalogJob)!;
      expect(spec.defaultPicks, job).toEqual(catalogJob.picks.map((pick) => pick.tag));
      // Profiles are by family: every catalog pick resolves to one, and other installed tags of a family do too.
      if (spec.models) for (const tag of spec.defaultPicks) expect(familyMatch(spec.models, tag), `${job}: ${tag}`).toBeDefined();
      expect(spec.sources.length, job).toBeGreaterThan(0);
      for (const profile of Object.values(spec.models ?? {})) {
        expect(profile.sources.every((source) => source.startsWith("https://")), job).toBe(true);
        for (const value of Object.values(profile.options ?? {})) expect(typeof value).toBe("number");
      }
    }
  });

  it("asks the coding model for exact SEARCH/REPLACE blocks", () => {
    const system = LOCAL_JOBS.coding.systemPrompt!;
    expect(system).toContain("<<<<<<< SEARCH\nexact current lines (5 to 30, unique in the file, indentation exact)\n=======\nreplacement lines\n>>>>>>> REPLACE");
    expect(system).toMatch(/whole file/);
    expect(LOCAL_JOBS.summarize.systemPrompt).toMatch(/"not stated"; never guess/);
  });
});

describe("job model pick", () => {
  it("runs the first pick that is installed and fits, skipping one short of memory", () => {
    expect(pickJobModel("ocr", installed("glm-ocr:latest", "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M"), 8 * GB)).toBe("hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M");
    // Nemotron (13.5 GB) comes first among those installed but has no room; bonsai-2:9b (10.3 GB) does.
    expect(pickJobModel("coding", installed("hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M", "bonsai-2:9b"), 12 * GB)).toBe("bonsai-2:9b");
    expect(pickJobModel("image-recognition", installed("lfm3-omni:2b-q4_K_M"), 6 * GB)).toBe("lfm3-omni:2b-q4_K_M");
    // Unknown free memory (serve down) is not "no room".
    expect(pickJobModel("classify", installed("strands-decider:2b-nvfp4"), null)).toBe("strands-decider:2b-nvfp4");
    // summarize runs the coding picks.
    expect(pickJobModel("summarize", installed("bonsai-2:9b"), 12 * GB)).toBe("bonsai-2:9b");
  });

  it("refuses with each pick's memory need and what is free, or when the inventory is unreadable", () => {
    let refusal: any;
    try { pickJobModel("ocr", installed("glm-ocr:latest"), 2.1 * GB); } catch (error) { refusal = JSON.parse((error as Error).message); }
    expect(refusal).toMatchObject({ code: "job_no_model_fits", job: "ocr", available_gb: 2.1 });
    expect(refusal.picks).toEqual([
      { tag: "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M", memory_gb: 3.8, installed: false, fits_now: false },
      { tag: "glm-ocr:latest", memory_gb: 5.8, installed: true, fits_now: false },
    ]);
    expect(refusal.error).toMatch(/free: 2\.1 GB.*glm-ocr:latest \(needs 5\.8 GB, installed\)/);
    expect(() => pickJobModel("ocr", null, 8 * GB)).toThrow(/could not be read.*pass `model`/);
  });
});

describe("job defaults", () => {
  it("fills the OCR prompt, and the caller's prompt wins", () => {
    const filled = applyJob("ocr", {}, "glm-ocr:latest");
    expect(filled.fields).toMatchObject({ task: "vision", model: "glm-ocr:latest", prompt: "Text Recognition:" });
    expect(filled.profile).toEqual({ job: "ocr", model: "glm-ocr:latest", applied: ["task", "model", "prompt"] });
    expect(applyJob("ocr", { prompt: OCR_PROMPTS.table }, "glm-ocr:latest").fields.prompt).toBe("Table Recognition:");
  });

  it("applies a model's documented system prompt and options, merged under the caller's", () => {
    const omni = applyJob("image-recognition", { prompt: "Which bar is tallest?", options: { num_predict: 64 } }, "lfm3-omni:2b-q4_K_M");
    expect(omni.fields).toMatchObject({
      systemPrompt: "Answer questions about the provided image. Base every answer only on what can be seen.",
      options: { temperature: 0, num_predict: 64 },
    });
    expect(omni.fields.think).toBeUndefined();
    const vl = applyJob("image-recognition", { prompt: "Which bar?", options: { temperature: 0.5 } }, "lfm3-vl:2b-q4_K_M");
    expect(vl.fields.systemPrompt).toBe(LOCAL_JOBS["image-recognition"].systemPrompt);
    expect(vl.fields.options).toEqual({ temperature: 0.5, top_p: 1, top_k: 50, min_p: 0, repeat_penalty: 1.1 });
  });

  it("turns bonsai-2:9b's thinking off with its non-thinking preset, unless the caller chose thinking", () => {
    const off = applyJob("coding", { prompt: "Rename x to y." }, "bonsai-2:9b");
    expect(off.fields).toMatchObject({ task: "coding", think: false, options: { temperature: 0.7, top_p: 0.8, top_k: 20, presence_penalty: 1.5 } });
    expect(off.fields.systemPrompt).toContain(">>>>>>> REPLACE");
    expect(off.profile.applied).toEqual(["task", "model", "systemPrompt", "think", "options"]);
    const thinking = applyJob("coding", { prompt: "Rename x to y.", think: true }, "bonsai-2:9b");
    expect(thinking.fields.think).toBe(true);
    expect(thinking.fields.options).toBeUndefined();
  });

  it("needs the caller's prompt where the job has no default, and leaves a caller's messages whole", () => {
    expect(() => applyJob("coding", {}, "lfm3:1.7b-q4_K_M")).toThrow(/needs prompt: the change to make/);
    expect(() => applyJob("image-recognition", {}, "lfm3-vl:2b-q4_K_M")).toThrow(/needs prompt: your question/);
    const messages = [{ role: "user", content: "hi" }];
    const own = applyJob("summarize", { messages }, "lfm3:1.7b-q4_K_M");
    expect(own.fields.prompt).toBeUndefined();
    expect(own.fields.systemPrompt).toBeUndefined();
    expect(applyJob("summarize", {}, "lfm3:1.7b-q4_K_M").fields.prompt).toBe("Summarize the text below.");
  });

  it("refuses a task that contradicts the job, and sets only routing fields for a preview", () => {
    expect(() => applyJob("ocr", { task: "completion" }, "glm-ocr:latest")).toThrow(/job "ocr" runs task "vision"/);
    expect(applyJob("ocr", {}, "glm-ocr:latest", true).fields).toEqual({ task: "vision", model: "glm-ocr:latest" });
  });

  it("adds each embedding family's documented prefixes once", () => {
    const gemma = applyJob("find-code", { nearest: { query: "saving a long path crashes", top: 5 } }, "embeddinggemma-2:740m-q8_0");
    expect(gemma.fields.nearest).toEqual({ query: "task: code retrieval | query: saving a long path crashes", top: 5, documentPrefix: "title: none | text: " });
    const nomic = applyJob("find-code", { nearest: { query: "search_query: already prefixed" } }, "nomic-embed-text:v1.5");
    expect(nomic.fields.nearest).toEqual({ query: "search_query: already prefixed", documentPrefix: "search_document: " });
    expect(nomic.profile.applied).not.toContain("nearest.query prefix");
    // Duplicates use the measured search_document: on every item; an item already prefixed keeps it.
    const dupes = applyJob("duplicates", { input: ["a", "search_document: b"] }, "nomic-embed-text:v1.5");
    expect(dupes.fields.nearest).toEqual({ documentPrefix: "search_document: " });
    expect(dupes.fields.input).toEqual(["search_document: a", "search_document: b"]);
    expect(() => applyJob("image-search", {}, "embeddinggemma-2:740m-q8_0")).toThrow(/needs nearest.query/);
    // An unknown family gets no prefix rather than a guessed one.
    expect(applyJob("find-code", { nearest: { query: "q" } }, "mxbai-embed-large:latest").fields.nearest).toEqual({ query: "q" });
  });
});

describe("run_task with job", () => {
  const receipt = (model: string, content: string) => JSON.stringify({
    route: { selected_model: model }, selected_model: model, response: { model, message: { role: "assistant", content } },
    metrics: { prompt_tokens: 10, output_tokens: 5 },
  });
  const extra = { signal: new AbortController().signal };
  const call = (args: Record<string, unknown>) => fixture.handlers.get("run_task")!(args, extra);

  beforeAll(() => {
    registerRunTaskTool({ registerTool: (name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) => {
      fixture.handlers.set(name, handler);
    } } as unknown as McpServer);
  });
  beforeEach(() => {
    fixture.live = vi.fn(async () => [installed("lfm3:1.7b-q4_K_M", "glm-ocr:latest"), 8 * GB]);
    fixture.run = vi.fn(async (_target: string, request: any) => receipt(request.model, "- step 2 failed: exit 1"));
  });

  it("sends the picked model with the job's system prompt and reports what it filled in", async () => {
    const result = await call({ job: "summarize", prompt: "Which steps failed?" });
    expect(result.isError).toBeFalsy();
    const sent = fixture.run.mock.calls[0][1];
    expect(sent).toMatchObject({ task: "completion", model: "lfm3:1.7b-q4_K_M", request_options: { options: { temperature: 0.1, top_k: 50, repeat_penalty: 1.1 } } });
    expect(sent.messages[0]).toMatchObject({ role: "system", content: LOCAL_JOBS.summarize.systemPrompt });
    expect(sent.messages[1]).toMatchObject({ role: "user", content: "Which steps failed?" });
    expect(result.structuredContent.job_profile).toEqual({ job: "summarize", model: "lfm3:1.7b-q4_K_M", applied: ["task", "model", "systemPrompt", "options"] });
  });

  it("lets an explicit model skip the pick, and refuses when no pick fits", async () => {
    await call({ job: "ocr", model: "glm-ocr:latest", images: ["aW1n"] });
    expect(fixture.live).not.toHaveBeenCalled();
    expect(fixture.run.mock.calls[0][1]).toMatchObject({ task: "vision", model: "glm-ocr:latest", prompt: "Text Recognition:" });
    fixture.live = vi.fn(async () => [installed("glm-ocr:latest"), 1 * GB]);
    const refused = await call({ job: "ocr", images: ["aW1n"] });
    expect(refused.isError).toBe(true);
    expect(refused.content[0].text).toMatch(/no catalog pick is installed and fits now \(free: 1\.0 GB\)/);
    expect(refused.structuredContent).toMatchObject({ code: "job_no_model_fits", available_gb: 1 });
  });

  it("applies the job to every batch item, with one pick, and reports it once", async () => {
    fixture.run = vi.fn(async () => JSON.stringify({ deferred: true, job: { id: crypto.randomUUID(), status: "queued" } }));
    const result = await call({ job: "ocr", defer: true, tasks: [
      { id: "a", independent: true, task: { images: ["aW1n"] } },
      { id: "b", independent: true, task: { images: ["aW1n"], prompt: "Table Recognition:" } },
    ] });
    expect(result.isError).toBeFalsy();
    expect(fixture.live).toHaveBeenCalledTimes(1);
    const prompts = fixture.run.mock.calls.map(([, request]: any[]) => [request.model, request.prompt]);
    expect(prompts).toEqual([["glm-ocr:latest", "Text Recognition:"], ["glm-ocr:latest", "Table Recognition:"]]);
    expect(result.structuredContent.job_profile).toMatchObject({ job: "ocr", model: "glm-ocr:latest" });
    const ranked = await call({ job: "duplicates", tasks: [{ id: "a", independent: true, task: { input: "x" } }] });
    expect(ranked.content[0].text).toMatch(/ranks every item in one call with `nearest`; send it without `tasks`/);
  });

  it("writes a saveAs handoff a later task can read, and refuses saveAs without a finished answer", async () => {
    const result = await call({ job: "summarize", prompt: "Which steps failed?", saveAs: "build-failures" });
    const file = result.structuredContent.handoff_file as string;
    expect(file).toBe(`${process.env.FREELLAMA_HOME}/handoffs/build-failures.md`);
    expect(result.content[0].text).toContain(`handoff_file: ${file}`);
    const body = readFileSync(file, "utf8");
    expect(body).toMatch(/^# build-failures\n\nFrom run_task job summarize, model lfm3:1\.7b-q4_K_M, \d{4}-/);
    expect(body).toContain("## Summary\n\n- step 2 failed: exit 1\n\n## Result\n\n- step 2 failed: exit 1\n");
    expect((await call({ prompt: "x", saveAs: "x", defer: true })).content[0].text).toMatch(/not valid with `preview` or `defer`/);
    expect((await call({ saveAs: "x", tasks: [{ id: "a", independent: true, task: { prompt: "x" } }] })).content[0].text).toMatch(/not valid with `tasks`/);
  });
});

describe("family profiles and model-aware prompts", () => {
  it("covers other tags, sizes and quantizations of a family, preferring the longest key", () => {
    const coding = LOCAL_JOBS.coding.models;
    expect(familyMatch(coding, "lfm3:latest")).toBe(familyMatch(coding, "lfm3:1.7b-q4_K_M"));
    expect(familyMatch(coding, "LFM3:1.7B")).toBeDefined();
    expect(familyMatch(coding, "bonsai-2:27b-q8_0")?.options?.temperature).toBe(1);
    expect(familyMatch(coding, "bonsai-2:9b-q4_K_M")?.think).toBe(false);
    expect(familyMatch(coding, "qwen3.8:27b")?.think).toBe(false);
    expect(familyMatch(coding, "hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q8_0")).toBeDefined();
    expect(familyMatch(coding, "lfm3-vl:2b")).toBeDefined();
    expect(familyMatch(LOCAL_JOBS["image-recognition"].models, "lfm3-vl:2b")).toBe(LOCAL_JOBS["image-recognition"].models!["lfm3-vl"]);
    expect(familyMatch(coding, "llama3:8b")).toBeUndefined();
  });

  it("asks OCR-trained models with their trained prefix and other vision models in plain words", () => {
    const prompt = LOCAL_JOBS.ocr.prompt!;
    expect(prompt({ model: "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q8_0" })).toBe("Text Recognition:");
    expect(prompt({ model: "glm-ocr:latest", kind: "table" })).toBe("Table Recognition:");
    expect(prompt({ model: "qwen3.8:27b" })).toMatch(/^Transcribe all text in the image exactly/);
    expect(prompt({ model: "lfm3-vl:2b", kind: "formula" })).toMatch(/LaTeX/);
  });
});

