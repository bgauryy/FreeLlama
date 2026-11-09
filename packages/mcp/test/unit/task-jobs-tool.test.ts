import { describe, expect, it, vi } from "vitest";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const job = vi.hoisted(() => ({ value: {} as Record<string, unknown> }));
vi.mock("../../src/serve-client.js", () => ({
  getTaskJob: async () => JSON.stringify({ job: job.value }),
  listTaskJobs: async () => "{}", cancelTaskJob: async () => "{}", removeTaskJob: async () => "{}",
}));
// Save into a throwaway store: the wrapper saves every result.
const root = await mkdtemp(path.join(tmpdir(), "task-jobs-"));
process.env.FREELLAMA_MCP_RESPONSES_DIR = root;

const { registerTaskJobsTool } = await import("../../src/tools/task-jobs.js");
const { finishResult } = await import("../../src/tool-results.js");
const { ResponseStore } = await import("../../src/response-store.js");

type Handler = (args: Record<string, unknown>) => Promise<{ content: { text: string }[]; structuredContent: Record<string, any> }>;
/** Call the registered task_jobs handler and shape its result the way withToolResults does. */
async function get(args: Record<string, unknown>) {
  let handler!: Handler;
  registerTaskJobsTool({ registerTool: (_n: string, _c: unknown, h: Handler) => { handler = h; } } as never);
  const store = new ResponseStore(await mkdtemp(path.join(tmpdir(), "responses-")), 1024 * 1024);
  const input = { action: "get", jobId: "11111111-1111-4111-8111-111111111111", ...args };
  return finishResult("task_jobs", input, await handler(input), 1, "t", store as never) as Promise<Awaited<ReturnType<Handler>>>;
}
const size = (result: { content: unknown; structuredContent: unknown }) => JSON.stringify({ content: result.content, structuredContent: result.structuredContent }).length;
const receipt = (answer: string) => ({
  selected_model: "m", response: { message: { content: answer, thinking: "t".repeat(3000) } },
  telemetry: { host: "h".repeat(4000) }, execution: { placement: "gpu", resource_admission: { snapshot: "s".repeat(3000) } },
});

describe("task_jobs get", () => {
  it("shortens a finished job's long answer and compacts its receipt, keeping the whole in the saved file", async () => {
    const answer = Array.from({ length: 40 }, (_, i) => `line ${i} ${"x".repeat(300)}`).join("\n");
    job.value = { id: "j", status: "completed", result: receipt(answer) };
    const auto = await get({});
    const whole = await get({ output: "inline" });
    console.log(`task_jobs get 12k answer: inline ${size(whole)} chars, auto ${size(auto)} chars`);
    expect(size(auto)).toBeLessThan(4_000);
    expect(size(whole)).toBeGreaterThan(12_000);
    expect(auto.content[0]!.text).not.toContain("line 39");
    expect(auto.structuredContent.output.mode).toBe("auto");
    expect(auto.structuredContent.job.result.telemetry).toBeUndefined();
    expect(auto.structuredContent.response_file).toEqual(expect.any(String));
  });

  it("returns a short answer whole, with the receipt compacted", async () => {
    job.value = { id: "j", status: "completed", result: receipt("Done") };
    const result = await get({});
    expect(result.content[0]!.text).toContain("Done");
    expect(result.structuredContent.output).toBeUndefined();
    expect(result.structuredContent.job.result.response.message.content).toBe("Done");
    expect(result.structuredContent.job.result.telemetry).toBeUndefined();
    expect(size(result)).toBeLessThan(1_500);
  });

  it("leaves a job without a result alone", async () => {
    job.value = { id: "j", status: "running" };
    const result = await get({});
    expect(result.structuredContent.job).toEqual({ id: "j", status: "running" });
  });
});
