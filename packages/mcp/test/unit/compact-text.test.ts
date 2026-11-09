// The text of results that carry no `summary`: compact, factual, and bounded. The handlers are
// driven with the native/serve boundary mocked. structuredContent stays the whole object, except
// models installed/resident, which return a slim per-model set unless includeVerbose is true.
import { beforeEach, describe, expect, it, vi } from "vitest";

const upstream = vi.hoisted(() => ({
  status: "", usage: "", route: "", models: "", runTask: "",
}));
vi.mock("../../src/native.js", () => ({
  status: async () => upstream.status,
  usage: async () => upstream.usage,
  doctor: async () => "{}",
  health: async () => "{}",
  machine: async () => "{}",
}));
vi.mock("../../src/serve-client.js", () => ({
  route: async () => upstream.route,
  listModels: async () => upstream.models,
  runTaskRequest: async () => upstream.runTask,
  cancelTaskJob: async () => "{}",
  getTaskJob: async () => "{}",
}));
vi.mock("../../src/serve.js", () => ({ ensureServe: async () => undefined }));

import { registerDoctorTool } from "../../src/tools/doctor.js";
import { registerModelsTool } from "../../src/tools/models.js";
import { registerRunTaskTool } from "../../src/tools/run-task.js";
import { RESULT_TEXT_LIMIT } from "../../src/helpers.js";

type Handler = (args: Record<string, unknown>, extra?: unknown) => Promise<{ content: { text: string }[]; structuredContent?: Record<string, any>; isError?: boolean }>;
function handlerOf(register: (server: never) => void): Handler {
  let handler: Handler | undefined;
  register({ registerTool: (_name: string, _config: unknown, callback: Handler) => { handler = callback; } } as never);
  return handler!;
}
const extra = { signal: new AbortController().signal, sendNotification: async () => undefined, _meta: {} };
const text = (result: { content: { text: string }[] }) => result.content[0]!.text;

const models = Array.from({ length: 40 }, (_, i) => ({
  name: `model-${i}:7b`, size: 4_000_000_000, capabilities: ["completion", "tools"], resident: i === 0,
  execution: { placement: "gpu", upstream: "http://127.0.0.1:11434", observation: { blob: "x".repeat(200) } },
  benchmark: { blob: "y".repeat(300) },
}));

beforeEach(() => {
  upstream.status = JSON.stringify({
    status: "ok",
    host: { status: "ready", holding: false, effective_available_bytes: 8 * 2 ** 30, total_memory_bytes: 32 * 2 ** 30,
      memory_pressure: "normal", load_average_one_minute: 5.7, reasons: ["low_available_memory"], gpu_activity: { blob: "z".repeat(5000) } },
    backends: { gpu: { admission: { in_flight: 1, queue_depth: 2, queue_limit: 16, slots_available: 1, slots_total: 2 } } },
    loaded_models: [{ name: "lfm3:1.7b" }], task_jobs: { jobs: [] },
    usage_today: { tasks: 68, errors: 22, prompt_tokens: 506560, output_tokens: 645, queue_wait_ms: 598602 },
    ollama: { config: { blob: "w".repeat(5000) } },
  });
  upstream.usage = JSON.stringify({
    window_days: 7, totals: { tasks: 71, errors: 25, prompt_tokens: 100, output_tokens: 5, busy_ms: 2000, queue_wait_ms: 1000 },
    by_model: Object.fromEntries(Array.from({ length: 30 }, (_, i) => [`m${i}`, { tasks: i, errors: 0, prompt_tokens: 1, output_tokens: 1 }])),
    by_day: [{ day: "2026-11-07", models: { blob: "q".repeat(5000) } }],
  });
  upstream.models = JSON.stringify({ models });
  upstream.route = JSON.stringify({
    task: "completion", selected_model: "bonsai-2:27b", resident: false, confidence: "low", evidence: "capability_metadata_only",
    context_window_fit: "fits_advertised_window", options: { num_ctx: 16384 }, reasons: ["installed", "capability_only_fallback"],
    rejected: [{ model: "glm-ocr:latest" }], execution: { backend: "primary", requested_processor: "gpu", resource_assessment: { status: "held_insufficient_capacity" }, blob: "b".repeat(6000) },
  });
});

describe("doctor", () => {
  const doctor = handlerOf(registerDoctorTool);
  it("status is a few lines with the facts, not cut JSON", async () => {
    const result = await doctor({ view: "status" });
    const out = text(result);
    expect(out.length).toBeLessThan(900);
    expect(out).toContain("8.0 GiB free of 32 GiB");
    expect(out).toContain("low_available_memory");
    expect(out).toContain("queue 2/16");
    expect(out).toContain("lfm3:1.7b");
    expect(out).not.toContain("truncated");
    expect(result.structuredContent!.ollama.config.blob).toHaveLength(5000);
  });
  it("usage lists the busiest models and stays bounded", async () => {
    const result = await doctor({ view: "usage" });
    const lines = text(result).split("\n");
    expect(lines[0]).toContain("last 7 day(s): 71 tasks, 25 errors");
    expect(lines[1]).toMatch(/^m29: 29 tasks/);
    expect(lines.length).toBeLessThanOrEqual(12);
    expect(text(result)).not.toContain("truncated");
  });
});

describe("models", () => {
  const modelsTool = handlerOf(registerModelsTool);
  it("installed is one line per model, under the text cap, with the whole list structured", async () => {
    const result = await modelsTool({ view: "installed" });
    const out = text(result);
    expect(out.split("\n")[0]).toBe("40 installed model(s), 1 resident:");
    expect(out).toContain("model-3:7b  3.7 GiB  completion/tools  placement gpu");
    expect(out).not.toContain("truncated");
    expect(out.length).toBeLessThan(RESULT_TEXT_LIMIT);
    expect(result.structuredContent!.models).toHaveLength(40);
  });
  it("resident lists only loaded models", async () => {
    const result = await modelsTool({ view: "resident" });
    expect(text(result)).toMatch(/^1 resident model\(s\):\nmodel-0:7b/);
  });
  it("explains that limit and cursor are for library and raw", async () => {
    const result = await modelsTool({ view: "installed", limit: 30 });
    expect(result.isError).toBe(true);
    expect(text(result)).toBe('`limit` and `cursor` apply only to views "library" and "raw", not "installed".');
  });
});

describe("run_task", () => {
  const runTask = handlerOf(registerRunTaskTool);
  it("preview is a short routing decision", async () => {
    const result = await runTask({ preview: true, task: "completion" }, extra);
    const out = text(result);
    expect(out).toContain("route: completion -> bonsai-2:27b (not resident); confidence low");
    expect(out).toContain("held_insufficient_capacity");
    expect(out).toContain("Nothing was executed");
    expect(out.length).toBeLessThan(700);
    expect(result.structuredContent!.execution.blob).toHaveLength(6000);
  });
  it("a deferred acknowledgement is one line naming the job", async () => {
    upstream.runTask = JSON.stringify({ deferred: true, job: { id: "job-1", task: "completion", requested_model: "lfm3:1.7b", status: "queued" } });
    const result = await runTask({ prompt: "hi", model: "lfm3:1.7b", defer: true }, extra);
    expect(text(result)).toBe("job-1 completion lfm3:1.7b queued; poll task_jobs get");
    expect(result.structuredContent!.deferred).toBe(true);
  });
  it("a deferred batch is one line per item", async () => {
    upstream.runTask = JSON.stringify({ deferred: true, job: { id: "job-x", task: "completion", status: "queued" } });
    const result = await runTask({ defer: true, tasks: [
      { id: "a", independent: true, task: { prompt: "one" } },
      { id: "b", independent: true, task: { prompt: "two" } },
    ] }, extra);
    expect(text(result).split("\n").slice(0, 2)).toEqual(["a: job-x queued", "b: job-x queued"]);
    expect(result.structuredContent!.jobs).toHaveLength(2);
  });
});
