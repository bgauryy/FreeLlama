import { describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  const { mkdtempSync } = process.getBuiltinModule("node:fs") as typeof import("node:fs");
  const { tmpdir } = process.getBuiltinModule("node:os") as typeof import("node:os");
  const { join } = process.getBuiltinModule("node:path") as typeof import("node:path");
  // Default output files land under ${FREELLAMA_HOME}/tmp: config.ts reads it when first imported.
  process.env.FREELLAMA_HOME = mkdtempSync(join(tmpdir(), "freellama-output-home-"));
});

import { AUTO_INLINE_CHARS, finishResult, SAVED_TEXT_SECTION, withTiming } from "../../src/tool-results.js";
import { OUTPUT_TMP_DIR } from "../../src/handoffs.js";
import { readResponse } from "../../src/tools/read-response.js";
import { ResponseStore } from "../../src/response-store.js";
import { mkdir, mkdtemp, readdir, readFile, realpath, rm, stat, utimes, writeFile } from "node:fs/promises";
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

  it.each([false, true])("keeps routing reasons and completion totals compact (batch=%s)", async (batch) => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const store = new ResponseStore(root, 1024 * 1024);
    const planning = {
      mode: "qualified_resident_idle", applied: false, snapshot_only: true,
      candidates: [
        { model: "gpu", backend: "gpu", status: "qualified_resident_idle_estimate", estimated_completion_ns: 0,
          profile_id: "gpu-profile", queue_ns: 0, prefill_ns: 0, decode_ns: 0 },
        { model: "cpu", backend: "cpu", status: "insufficient_phase_history", estimated_completion_ns: null,
          profile_id: null, queue_ns: null, prefill_ns: null, decode_ns: null },
      ],
      payload_fit_rejections: [{ model: "small", reason: "execution_payload_context_does_not_fit" }],
      minimum_improvement_percent: 10, limitations: "full detail", fallback: "existing policy",
    };
    const receipt = {
      selected_model: "gpu", response: { message: { content: "answer" } },
      execution: { backend: "gpu", reason: "existing_policy", completion_planning: planning },
    };
    const payload = batch ? { results: [{ id: "item-1", ok: true, response: receipt }] } : receipt;
    const result = await finishResult("run_task", {}, { structuredContent: payload }, 1, "t", store);
    const content = result.structuredContent as Record<string, any>;
    const inline = batch ? content.results[0].response : content;
    expect(inline.execution).toEqual({
      backend: "gpu", reason: "existing_policy",
      completion_planning: {
        mode: "qualified_resident_idle", applied: false, snapshot_only: true,
        candidates: [
          { model: "gpu", backend: "gpu", status: "qualified_resident_idle_estimate", estimated_completion_ns: 0 },
          { model: "cpu", backend: "cpu", status: "insufficient_phase_history", estimated_completion_ns: null },
        ],
        payload_fit_rejections: planning.payload_fit_rejections,
      },
    });
    const saved = JSON.parse(await readFile(content.response_file, "utf8")).result.structuredContent;
    const fullReceipt = batch ? saved.results[0].response : saved;
    expect(fullReceipt.execution.completion_planning).toEqual(planning);
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
    const embeddingIdentity = { model_digest: "sha256:exact", backend: "cpu", upstream: "http://cpu", dimensions: 16, status: "verified" };
    const receipt = { selected_model: "e", route: { selected_model: "e" }, response: { model: "e", embeddings: vectors },
      embedding_identity: embeddingIdentity, accepted_job_ids: ["job-one"], embedding_slices: [{ metrics: { prompt_tokens: 3 } }] };
    const result = await finishResult("run_task", { input: "x" }, { structuredContent: receipt }, 1, "t", await store());
    const content = result.structuredContent as Record<string, any>;
    expect(content.response.embeddings).toBeUndefined();
    expect(content.response.embeddings_omitted).toMatchObject({ count: 1, dimensions: 16 });
    expect(content.response.embeddings_omitted.note).toContain("read_response");
    expect(content.sections).toEqual(["receipt", "embeddings"]);
    expect(content.embedding_identity).toEqual(embeddingIdentity);
    expect(content.accepted_job_ids).toEqual(["job-one"]);
    expect(content.embedding_slices).toBeUndefined();
    const savedRecord = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(savedRecord.result.structuredContent.response.embeddings).toEqual(vectors);
    expect(savedRecord.result.structuredContent.embedding_slices).toEqual(receipt.embedding_slices);
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

describe("answer output: auto, inline, file and outputPath", () => {
  const label = "[m:1 · cold load] ";
  const answerOf = (size: number) => `${label}${Array.from({ length: 40 }, (_, i) => `line ${i} ${"x".repeat(Math.max(0, Math.floor(size / 40) - 12))}`).join("\n")}`;
  const withLines = (text: string) => `${text}\ncheck: 2 numbers are not in the source\ncheck: close match\nhandoff_file: /h/notes.md`;
  const resultOf = (text: string) => ({
    content: [{ type: "text" as const, text }],
    structuredContent: { selected_model: "m:1", response: { message: { content: text } } },
  });
  const run = (tool: string, args: Record<string, unknown>, text: string, store?: Pick<ResponseStore, "save">) =>
    finishResult(tool, args, resultOf(text), 1, "t", store);
  const storeIn = async () => new ResponseStore(await mkdtemp(path.join(tmpdir(), "responses-")), 1024 * 1024);
  const textOf = (result: { content: { text: string }[] }) => result.content[0]!.text;
  const off = { save: async () => undefined };
  const scratch = async () => mkdtemp(path.join(tmpdir(), "outputs-"));
  const outputs = async () => (await readdir(OUTPUT_TMP_DIR).catch(() => [])).filter((name) => name.endsWith(".md"));
  /** Make the default output directory unusable (a regular file where it should be) while `body` runs. */
  const withBlockedOutputDir = async (body: () => Promise<void>) => {
    await rm(OUTPUT_TMP_DIR, { recursive: true, force: true });
    await writeFile(OUTPUT_TMP_DIR, "not a directory");
    try {
      await body();
    } finally {
      await rm(OUTPUT_TMP_DIR, { force: true });
    }
  };

  it("returns an answer of at most AUTO_INLINE_CHARS whole and a longer one as a short view with its output file", async () => {
    expect(AUTO_INLINE_CHARS).toBe(8000);
    const store = await storeIn();
    const exact = "a".repeat(AUTO_INLINE_CHARS);
    const before = await outputs();
    const whole = await run("run_task", {}, exact, store);
    expect(textOf(whole as never)).toBe(exact);
    expect((whole.structuredContent as Record<string, any>).output).toBeUndefined();
    expect(await outputs()).toEqual(before);

    const long = withLines(answerOf(AUTO_INLINE_CHARS + 500));
    const short = await run("run_task", { output: "auto" }, long, store);
    const content = short.structuredContent as Record<string, any>;
    const lines = textOf(short as never).split("\n");
    expect(lines[0]).toBe("[m:1 · cold load]");
    expect(lines[1]).toMatch(/^line 0 x+/);
    expect(textOf(short as never)).not.toContain("line 39");
    expect(textOf(short as never)).toContain("check: 2 numbers are not in the source\ncheck: close match\nhandoff_file: /h/notes.md");
    expect(lines.at(-1)).toBe(`output_file: ${content.output_file} (the full answer, ${long.length} chars)`);
    expect(textOf(short as never).length).toBeLessThan(1500);
    expect(content.output).toEqual({ mode: "auto", chars: long.length, file: content.output_file });
    // structuredContent points at the file instead of repeating the answer.
    expect(content.response.message.content).toBe(`answer moved: output_file: ${content.output_file} (the full answer, ${long.length} chars)`);
    expect(path.dirname(content.output_file)).toBe(OUTPUT_TMP_DIR);
    expect(path.basename(content.output_file)).toMatch(/^\d{4}-\d\d-\d\dT\d\d-\d\d-\d\d-\d{3}Z-\d+-\d+\.md$/);
    expect(await readFile(content.output_file, "utf8")).toContain(`## Result\n\n${long.replace(label, "").trim()}\n`);
    expect((await stat(content.output_file)).mode & 0o777).toBe(0o600);
    expect(await outputs()).toContain(path.basename(content.output_file));
  });

  it("returns a 10k-char answer under 4,000 chars in total, with the whole text in the file and the saved response", async () => {
    const store = await storeIn();
    const long = answerOf(10_000);
    const receipt = { ...resultOf(long).structuredContent, results: [{ id: "a", ok: true, response: { response: { message: { content: long } } } }] };
    const input = { content: [{ type: "text" as const, text: long }], structuredContent: receipt };
    const inline = await finishResult("run_task", { output: "inline" }, input, 1, "t", store);
    const auto = await finishResult("run_task", {}, input, 1, "t", store);
    const size = (result: unknown) => JSON.stringify({ content: (result as any).content, structuredContent: (result as any).structuredContent }).length;
    console.log(`10k answer total chars: inline ${size(inline)}, auto ${size(auto)}`);
    expect(size(inline)).toBeGreaterThan(20_000);
    expect(size(auto)).toBeLessThan(4_000);
    const content = auto.structuredContent as Record<string, any>;
    expect(JSON.stringify(content)).not.toContain("line 39");
    expect(content.results[0].response.response.message.content).toContain("answer moved: output_file:");
    expect(await readFile(content.output_file, "utf8")).toContain("line 39");
    const savedFile = JSON.parse(await readFile(content.response_file, "utf8"));
    expect(savedFile.result.structuredContent.response.message.content).toBe(long);
    expect(savedFile.result.structuredContent.results[0].response.response.message.content).toBe(long);
  });

  it("keeps host telemetry and nested metrics out of the inline receipt and in the file", async () => {
    const store = await storeIn();
    const receipt = { selected_model: "m", response: { message: { content: "pong" } }, execution: { placement: "gpu" }, telemetry: { host: "x".repeat(2000) },
      metrics: { eval_count: 3, load_duration_ns: 5, detail: { big: "y".repeat(500) } } };
    const result = await finishResult("run_task", {}, { structuredContent: receipt }, 1, "t", store);
    const content = result.structuredContent as Record<string, any>;
    expect(content.telemetry).toBeUndefined();
    expect(content.metrics).toEqual({ eval_count: 3, load_duration_ns: 5 });
    expect(JSON.parse(await readFile(content.response_file, "utf8")).result.structuredContent.telemetry.host).toHaveLength(2000);
  });

  it("gives parallel and same-millisecond answers different default files", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    try {
      const answers = await Promise.all([1, 2, 3].map((n) => run("run_task", { output: "file" }, `answer ${n}`, off)));
      const files = answers.map((answer) => (answer.structuredContent as Record<string, any>).output_file);
      expect(new Set(files).size).toBe(3);
    } finally {
      vi.useRealTimers();
    }
  });

  it("addresses the whole saved text with the JSON pointer it names", async () => {
    const root = await mkdtemp(path.join(tmpdir(), "responses-"));
    const store = new ResponseStore(root, 1024 * 1024);
    const long = withLines(answerOf(20_000));
    const short = await run("run_task", {}, long, store);
    const file = (short.structuredContent as Record<string, any>).response_file as string;
    const view = await readResponse({ file, section: SAVED_TEXT_SECTION, limit: 1_000_000 }, root) as { structuredContent: Record<string, any> };
    expect(view.structuredContent.value).toBe(long);
    expect(view.structuredContent.total).toBe(long.length);
  });

  it("keeps an over-threshold answer whole and writes nothing for output inline, and shortens a small one for output file", async () => {
    const store = await storeIn();
    const long = answerOf(12_000);
    const before = await outputs();
    expect(textOf(await run("run_task", { output: "inline" }, long, store) as never)).toBe(long);
    expect(await outputs()).toEqual(before);
    const small = await run("run_task", { output: "file" }, withLines("[m:1] short answer"), store);
    expect(textOf(small as never)).toMatch(/^\[m:1\]\nshort answer\ncheck: 2 numbers.*\ncheck: close match\nhandoff_file: \/h\/notes\.md\noutput_file: .*\.md \(the full answer, \d+ chars\)$/);
    expect((small.structuredContent as Record<string, any>).output.mode).toBe("file");
  });

  it("keeps the research verdict and citations in the short view", async () => {
    const store = await storeIn();
    const text = `${answerOf(12_000)}\n\nverification: escalate\ncited: src/a.ts, src/b.ts\n(m:1, 3 tool call(s))`;
    const short = textOf(await run("delegate_research", {}, text, store) as never);
    expect(short).toContain("verification: escalate\ncited: src/a.ts, src/b.ts\noutput_file: ");
    expect(short).not.toContain("tool call(s)");
  });

  it("leaves errors, previews, acknowledgements and other tools alone", async () => {
    const store = await storeIn();
    const long = answerOf(12_000);
    const before = await outputs();
    expect(textOf(await run("read_response", { output: "file" }, long, store) as never)).toBe(long);
    const failed = await finishResult("run_task", { output: "file" }, { ...resultOf(long), isError: true }, 1, "t", store);
    expect(textOf(failed as never)).toBe(long);
    expect(textOf(await run("run_task", { output: "file", preview: true }, long, store) as never)).toBe(long);
    expect(textOf(await run("run_task", { output: "file", defer: true }, long, store) as never)).toBe(long);
    const ack = await finishResult("run_task", { output: "file" }, { content: [{ type: "text", text: long }], structuredContent: { deferred: true, job: { id: "j", status: "queued" } } }, 1, "t", store);
    expect(textOf(ack as never)).toBe(long);
    expect(await outputs()).toEqual(before);
  });

  it("falls back to the saved response when the default file cannot be written, and keeps the answer whole when that is off too", async () => {
    const long = answerOf(12_000);
    await withBlockedOutputDir(async () => {
      const store = await storeIn();
      const saved = await run("run_task", {}, long, store);
      const content = saved.structuredContent as Record<string, any>;
      expect(textOf(saved as never)).toMatch(/output_file not written: /);
      expect(textOf(saved as never).split("\n")).toContain(`full answer (${long.length} chars): read_response{file:"${content.response_file}", section:"${SAVED_TEXT_SECTION}"}`);
      expect(content.output).toEqual({ mode: "auto", chars: long.length, file: content.response_file });
      expect(content.output_file).toBeUndefined();

      const result = await run("run_task", {}, long, off);
      expect(textOf(result as never).startsWith(`${long}\noutput_file not written: `)).toBe(true);
      expect(textOf(result as never)).toContain(`\nnot shortened (${long.length} chars): `);
      expect((result.structuredContent as Record<string, any>).output).toBeUndefined();
    });
    // An answer that was not going to shorten, with the store off, stays as it is.
    expect(textOf(await run("run_task", {}, "short", off) as never)).toBe("short");
  });

  it("writes outputPath as Markdown, creating directories, returns output_file, and points the short view at it", async () => {
    const dir = await scratch();
    const target = path.join(dir, "notes", "answer.md");
    const long = withLines(answerOf(12_000));
    const before = await outputs();
    const result = await run("run_task", { outputPath: target }, long, off);
    const content = result.structuredContent as Record<string, any>;
    const real = path.join(await realpath(dir), "notes", "answer.md");
    expect(content.output_file).toBe(real);
    expect(content.output).toEqual({ mode: "auto", chars: long.length, file: real });
    expect(textOf(result as never).split("\n").at(-1)).toBe(`output_file: ${real} (the full answer, ${long.length} chars)`);
    const body = await readFile(real, "utf8");
    expect(body.startsWith("# answer.md\n\nFrom FreeLlama, ")).toBe(true);
    expect(body).toContain(`## Result\n\n${long.replace(label, "").trim()}\n`);
    expect((await stat(real)).mode & 0o777).toBe(0o600);
    expect((await stat(path.dirname(real))).mode & 0o777).toBe(0o700);
    // The caller chose the file, so no default file is written.
    expect(await outputs()).toEqual(before);

    // inline: the text stays whole and the file is a copy; the caller's path is replaced, whoever wrote it.
    const copy = await run("run_task", { outputPath: target, output: "inline" }, "second answer", off);
    expect(textOf(copy as never)).toBe(`second answer\noutput_file: ${real}`);
    expect(await readFile(real, "utf8")).toContain("## Result\n\nsecond answer\n");
    const mine = path.join(dir, "mine.md");
    await writeFile(mine, "# my notes\n");
    await run("run_task", { outputPath: mine, output: "inline" }, "replaced", off);
    expect(await readFile(mine, "utf8")).toContain("## Result\n\nreplaced\n");
  });

  it("keeps the whole answer inline and names the error when the outputPath write fails", async () => {
    const dir = await scratch();
    const long = answerOf(12_000);
    // A directory is not a regular file: refused at the write, after the work, and the answer stays whole.
    const refused = await run("run_task", { outputPath: dir, output: "file" }, long, off);
    expect(textOf(refused as never)).toContain(`${long}\noutput_file not written: outputPath ${dir} exists and is not a regular file`);
    expect((refused.structuredContent as Record<string, any>).output_file).toBeUndefined();

    // A parent that is a file: with a store the short view points at the saved response.
    const taken = path.join(dir, "taken.md");
    await writeFile(taken, "x");
    const store = await storeIn();
    const blocked = await run("run_task", { outputPath: path.join(taken, "x.md") }, long, store);
    expect(textOf(blocked as never)).toMatch(/output_file not written: /);
    expect(textOf(blocked as never)).toMatch(/full answer \(\d+ chars\): read_response/);
  });
});
