import { describe, expect, it } from "vitest";
import { finishResult, withTiming } from "../../src/tool-results.js";
import { ResponseStore } from "../../src/response-store.js";
import { mkdir, mkdtemp, readdir, readFile, stat, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

/** The saved result files of one session directory, without its index. */
const saved = async (directory: string) => (await readdir(directory)).filter((name) => name.endsWith(".json")).sort();

describe("tool timing", () => {
  it("adds wall time and the receipt's phases to a structured result", () => {
    const result = withTiming({
      structuredContent: {
        admission: { total_wait_ms: 12 },
        metrics: { load_duration_ns: 6_100_000_000, prompt_duration_ns: 40_000_000, output_duration_ns: 900_000_000 },
      },
    }, 7_100, "2026-10-30T00:00:00.000Z");
    expect((result.structuredContent as unknown as { timing: unknown }).timing).toEqual({
      elapsed_ms: 7_100, started_at: "2026-10-30T00:00:00.000Z",
      phases: { queue_ms: 12, load_ms: 6_100, prompt_ms: 40, output_ms: 900 },
    });
  });

  it("keeps a result without phases to wall time", () => {
    const result = withTiming({ structuredContent: { models: [] } }, 5, "t");
    expect((result.structuredContent as unknown as { timing: unknown }).timing).toEqual({ elapsed_ms: 5, started_at: "t" });
  });

  it("puts an unstructured error's timing in metadata", () => {
    const result = withTiming<{ isError: boolean; structuredContent?: unknown; _meta?: Record<string, unknown> }>({ isError: true }, 3, "t");
    expect(result._meta).toEqual({ "freellama/timing": { elapsed_ms: 3, started_at: "t" } });
    expect(result.structuredContent).toBeUndefined();
  });

  it("saves the whole result, compacts a run_task receipt, and keeps the answer whole", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const store = new ResponseStore(root, 1024 * 1024);
    const answer = "x".repeat(50_000);
    const receipt = {
      selected_model: "m", response: { message: { content: answer }, done_reason: "stop" },
      route: { selected_model: "m", task: "completion", confidence: "low", reasons: ["installed"], rejected: [{ model: "n" }] },
      execution: { placement: "gpu", resource_admission: { snapshot: { big: true } } },
      admission: { total_wait_ms: 3 }, metrics: { load_duration_ns: 2_000_000 },
    };
    const result = await finishResult("run_task", { prompt: "p" }, { structuredContent: receipt }, 9, "t", store);
    const content = result.structuredContent as Record<string, any>;
    expect(content.response.message.content).toBe(answer);
    expect(content.route).toEqual({ selected_model: "m", task: "completion", confidence: "low", reasons: ["installed"] });
    // Where it ran stays; admission and host-telemetry snapshots move to the file.
    expect(content.execution).toEqual({ placement: "gpu" });
    expect(content.admission).toEqual({ total_wait_ms: 3 });
    expect(content.timing).toEqual({ elapsed_ms: 9, started_at: "t", phases: { queue_ms: 3, load_ms: 2 } });
    const saved = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(saved.result.structuredContent.execution.resource_admission.snapshot.big).toBe(true);
    const full = await finishResult("run_task", { receipt: "full" }, { structuredContent: receipt }, 1, "t", store);
    expect((full.structuredContent as Record<string, any>).execution.resource_admission).toEqual({ snapshot: { big: true } });
    const preview = await finishResult("run_task", { preview: true }, { structuredContent: receipt }, 1, "t", store);
    expect((preview.structuredContent as Record<string, any>).route.rejected).toEqual([{ model: "n" }]);
  });

  it("indexes once when saves race and counts files other sessions wrote", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const other = new ResponseStore(root, 10_000, new Date(0));
    await other.save("models", { pad: "z".repeat(1_400) });
    await other.save("models", { pad: "z".repeat(1_400) });
    const store = new ResponseStore(root, 4_000);
    await Promise.all([store.save("doctor", { pad: "a".repeat(1_400) }), store.save("doctor", { pad: "b".repeat(1_400) })]);
    // The other session's older file went first; its newest stays for the agent about to read it.
    const sessions = (await readdir(root)).sort();
    expect(sessions).toHaveLength(2);
    expect(await saved(path.join(root, sessions[0]!))).toEqual(["000002-models.json"]);
    expect((await saved(path.join(root, sessions[1]!))).at(-1)).toBe("000002-doctor.json");
  });

  it("keeps every session's newest file while the store stays under twice its size", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const other = new ResponseStore(root, 10_000, new Date(0));
    await other.save("models", { pad: "z".repeat(800) });
    const store = new ResponseStore(root, 1_000);
    await store.save("doctor", { pad: "a".repeat(800) });
    const sessions = (await readdir(root)).sort();
    expect(sessions).toHaveLength(2);
    expect(await saved(path.join(root, sessions[0]!))).toEqual(["000001-models.json"]);
  });

  it("drops another session's newest file once the store is over twice its size", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const other = new ResponseStore(root, 10_000, new Date(0));
    await other.save("models", { pad: "z".repeat(800) });
    const store = new ResponseStore(root, 500);
    const file = await store.save("doctor", { pad: "a".repeat(800) });
    // The other session had no files left, so its directory went with its last file.
    expect(await readdir(root)).toEqual([path.basename(path.dirname(file!))]);
    expect(await saved(path.dirname(file!))).toEqual(["000001-doctor.json"]);
  });

  it("keeps an append-only index per session and records evictions in it", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const store = new ResponseStore(root, 600);
    for (let index = 0; index < 4; index += 1) await store.save("run_task", { pad: "y".repeat(200), index }, { model: "m" });
    const [session] = await readdir(root);
    const lines = (await readFile(path.join(root, session!, "index.jsonl"), "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    expect(lines[0]).toMatchObject({ seq: 1, tool: "run_task", model: "m" });
    expect(typeof lines[0].bytes).toBe("number");
    expect(Number.isNaN(Date.parse(lines[0].created))).toBe(false);
    const removed = lines.filter((line) => line.removed === true).map((line) => line.seq);
    expect(removed.length).toBeGreaterThan(0);
    const live = (await saved(path.join(root, session!))).map((name) => Number.parseInt(name, 10));
    expect(live.some((seq) => removed.includes(seq))).toBe(false);
    expect((await stat(path.join(root, session!, "index.jsonl"))).mode & 0o777).toBe(0o600);
    expect((await stat(path.join(root, session!))).mode & 0o777).toBe(0o700);
  });

  it("drops files older than the age limit, and keeps them when the limit is 0", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    let now = Date.parse("2026-10-01T00:00:00Z");
    const old = new ResponseStore(root, 1024 * 1024, new Date(0), 7, () => now);
    await old.save("models", { a: 1 });
    await old.save("models", { a: 2 });
    now += 8 * 86_400_000;
    const keeping = new ResponseStore(root, 1024 * 1024, new Date(1), 0, () => now);
    await keeping.save("doctor", { b: 1 });
    expect((await readdir(root)).length).toBe(2);
    const expiring = new ResponseStore(root, 1024 * 1024, new Date(2), 7, () => now);
    await expiring.save("doctor", { c: 1 });
    // Both of the old session's files expired, its newest included; the recent ones stay.
    const sessions = (await readdir(root)).sort();
    expect(sessions).toHaveLength(2);
    expect(sessions.some((name) => name.startsWith("1970-01-01T00-00-00-000Z"))).toBe(false);
  });

  it("reads sessions written before the index existed by their files", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const legacy = path.join(root, "1970-01-01T00-00-00-000Z-1");
    await mkdir(legacy, { recursive: true });
    for (const name of ["000001-models.json", "000002-models.json"]) {
      await writeFile(path.join(legacy, name), "x".repeat(400));
      const hourAgo = new Date(Date.now() - 3_600_000);
      await utimes(path.join(legacy, name), hourAgo, hourAgo);
    }
    const store = new ResponseStore(root, 900);
    await store.save("doctor", { pad: "a".repeat(200) });
    expect(await saved(legacy)).toEqual(["000002-models.json"]);
  });

  it("drops the oldest files once the store is over its size, never the newest", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const store = new ResponseStore(root, 600);
    for (let index = 0; index < 5; index += 1) await store.save("doctor", { pad: "y".repeat(200), index });
    const [session] = await readdir(root);
    const files = await saved(path.join(root, session!));
    expect(files.length).toBeLessThan(5);
    expect(files.at(-1)).toBe("000005-doctor.json");
    expect(await new ResponseStore(root, 0).save("doctor", {})).toBeUndefined();
  });
});

describe("smart responses", () => {
  const store = async (bytes = 16 * 1024 * 1024) => new ResponseStore(await mkdtemp(path.join(tmpdir(), "responses-")), bytes);

  it("moves thinking out of the inline receipt, leaving its length", async () => {
    const thinking = "t".repeat(5_120);
    const receipt = { selected_model: "m", route: { selected_model: "m" }, response: { message: { role: "assistant", content: "a", thinking } } };
    const result = await finishResult("run_task", { prompt: "p" }, { structuredContent: receipt }, 1, "t", await store());
    const content = result.structuredContent as Record<string, any>;
    expect(content.response.message).toEqual({ role: "assistant", content: "a", thinking_chars: 5_120 });
    expect(content.sections).toEqual(["receipt", "thinking"]);
    const savedRecord = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(savedRecord.result.structuredContent.response.message.thinking).toBe(thinking);
    const full = await finishResult("run_task", { receipt: "full" }, { structuredContent: receipt }, 1, "t", await store());
    expect((full.structuredContent as Record<string, any>).response.message.thinking).toBe(thinking);
  });

  it("saves embedding vectors and omits them only from the inline view", async () => {
    const vectors = [Array.from({ length: 16 }, (_, index) => index / 16)];
    const receipt = { selected_model: "e", route: { selected_model: "e" }, response: { model: "e", embeddings: vectors } };
    const result = await finishResult("run_task", { input: "x" }, { structuredContent: receipt }, 1, "t", await store());
    const content = result.structuredContent as Record<string, any>;
    expect(content.response.embeddings).toBeUndefined();
    expect(content.response.embeddings_omitted).toMatchObject({ count: 1, dimensions: 16 });
    expect(content.response.embeddings_omitted.note).toContain("read_response");
    expect(content.sections).toEqual(["receipt", "embeddings"]);
    const savedRecord = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(savedRecord.result.structuredContent.response.embeddings).toEqual(vectors);
    const returned = await finishResult("run_task", { input: "x", returnEmbeddings: true }, { structuredContent: receipt }, 1, "t", await store());
    expect((returned.structuredContent as Record<string, any>).response.embeddings).toEqual(vectors);
  });

  it("keeps delegate_research's answer, grading and citations inline and the trail in the file", async () => {
    const payload = {
      adapter: "bash", answer: "in src/a.ts", summary: "s", verification: { recommendation: "verify" },
      citations: [{ step: 1, tool: "shell", path: "src/a.ts" }], toolCallCount: 1, successfulToolCallCount: 1,
      usage: { inputTokens: 10, outputTokens: 2 }, telemetry: { note: "n" }, contextManagement: { compactions: 0 },
      execution: { receipts: [{ big: true }] }, evidence: [{ step: 1 }], observations: [{ step: 1, result: "o".repeat(10_000) }],
    };
    const result = await finishResult("delegate_research", { question: "q" }, { structuredContent: payload }, 1, "t", await store());
    const content = result.structuredContent as Record<string, any>;
    expect(Object.keys(content).sort()).toEqual([
      "adapter", "answer", "citations", "response_file", "sections", "successfulToolCallCount", "summary", "timing",
      "toolCallCount", "usage", "verification",
    ]);
    expect(content.sections).toEqual(["observations", "evidence", "execution", "contextManagement", "telemetry"]);
    const savedRecord = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(savedRecord.result.structuredContent.observations[0].result).toHaveLength(10_000);
    const full = await finishResult("delegate_research", { receipt: "full" }, { structuredContent: payload }, 1, "t", await store());
    expect((full.structuredContent as Record<string, any>).observations).toHaveLength(1);
  });

  it("moves nothing out of the view when the store cannot save it", async () => {
    const receipt = { selected_model: "m", route: { selected_model: "m", rejected: [1] }, response: { message: { content: "a", thinking: "why" } } };
    const result = await finishResult("run_task", { prompt: "p" }, { structuredContent: receipt }, 1, "t", new ResponseStore("/unused", 0));
    const content = result.structuredContent as Record<string, any>;
    expect(content.response.message.thinking).toBe("why");
    expect(content.route.rejected).toEqual([1]);
    expect(content.response_file).toBeUndefined();
    expect(content.sections).toBeUndefined();
  });

  it("does not save read_response results again", async () => {
    const saves: string[] = [];
    const result = await finishResult("read_response", { file: "f" }, { structuredContent: { value: "v" } }, 1, "t", {
      save: async (tool) => { saves.push(tool); return "/x"; },
    });
    expect(saves).toEqual([]);
    expect((result.structuredContent as Record<string, any>).response_file).toBeUndefined();
  });
});
