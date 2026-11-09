// models installed/resident: default structuredContent is slim; includeVerbose returns full records.
import { beforeEach, describe, expect, it, vi } from "vitest";

const upstream = vi.hoisted(() => ({ models: "" }));
vi.mock("../../src/native.js", () => ({ machine: async () => "{}" }));
vi.mock("../../src/serve-client.js", () => ({ listModels: async () => upstream.models }));
vi.mock("../../src/serve.js", () => ({ ensureServe: async () => undefined }));

import { registerModelsTool } from "../../src/tools/models.js";

type Handler = (args: Record<string, unknown>) => Promise<{ content: { text: string }[]; structuredContent?: Record<string, any>; isError?: boolean }>;
function handlerOf(register: (server: never) => void): Handler {
  let handler: Handler | undefined;
  register({ registerTool: (_n: string, _c: unknown, callback: Handler) => { handler = callback; } } as never);
  return handler!;
}
const tool = handlerOf(registerModelsTool);
const size = (value: unknown) => JSON.stringify(value).length;

const models = Array.from({ length: 15 }, (_, i) => ({
  name: `model-${i}:7b`, digest: `sha256:${"a".repeat(64)}`, size: 4_000_000_000, capabilities: ["completion", "tools"],
  resident: i === 0, resident_size: i === 0 ? 5_000_000_000 : null, resident_context: i === 0 ? 8192 : null, advertised_context: 32768,
  resident_view: i === 0 ? { gpu_percent: 100, processor_label: "GPU", assigned: true } : undefined,
  execution: { placement: "gpu", upstream: "http://127.0.0.1:11434", backend: "primary", observation: { processor: "100% GPU", size: 5_000_000_000, size_vram: 5_000_000_000, source: "ollama_api_ps", status: "observed" } },
  benchmark: { blob: "y".repeat(300) }, policy_rank: { blob: "z".repeat(200) }, observed_loops: { completion: { loops: 0, runs: 1 } },
  thinking: { default: "medium", values: [false, "low", "medium"] }, context_default: { num_ctx: null, source: "unknown" },
}));

beforeEach(() => { upstream.models = JSON.stringify({ models }); });

describe("models payload size", () => {
  it("installed default keeps identifiers and shrinks the structured payload", async () => {
    const slim = await tool({ view: "installed" });
    const full = await tool({ view: "installed", includeVerbose: true });
    expect(slim.structuredContent!.models).toHaveLength(15);
    expect(slim.structuredContent!.models[0]).toEqual({
      name: "model-0:7b", size: 4_000_000_000, capabilities: ["completion", "tools"], resident: true, resident_size: 5_000_000_000,
      resident_context: 8192, advertised_context: 32768,
      // Observed placement stays: callers must not infer where a model runs (AGENTS.md).
      execution: { placement: "gpu", upstream: "http://127.0.0.1:11434",
        observation: { processor: "100% GPU", size: 5_000_000_000, size_vram: 5_000_000_000, source: "ollama_api_ps", status: "observed" } },
    });
    expect(slim.content[0]!.text).toBe(full.content[0]!.text);
    expect(size(slim.structuredContent)).toBeLessThan(size(full.structuredContent) / 2);
    // 15 models: ~250 chars of identifiers plus ~110 of observed placement each.
    expect(size(slim.structuredContent)).toBeLessThan(6_000);
  });
  it("installed includeVerbose returns the full records", async () => {
    const full = await tool({ view: "installed", includeVerbose: true });
    const first = full.structuredContent!.models[0];
    expect(first.benchmark).toEqual({ blob: "y".repeat(300) });
    expect(first.execution.observation).toBeDefined();
    expect(first.digest).toBeDefined();
    expect(first.knowledge).toBeDefined();
  });
  it("resident default is slim and verbose is full", async () => {
    const slim = await tool({ view: "resident" });
    const full = await tool({ view: "resident", includeVerbose: true });
    const entry = slim.structuredContent!.models[0];
    expect(slim.structuredContent!.models).toHaveLength(1);
    expect(entry.placement).toEqual({ gpu_percent: 100, assigned: true, processor: "GPU" });
    expect(entry.benchmark).toBeUndefined();
    expect(full.structuredContent!.models[0].benchmark).toBeDefined();
    expect(slim.content[0]!.text).toBe(full.content[0]!.text);
    expect(size(slim.structuredContent)).toBeLessThan(size(full.structuredContent));
  });
  it("rejects includeVerbose on views that have no verbose form", async () => {
    const result = await tool({ view: "raw", includeVerbose: true });
    expect(result.isError).toBe(true);
    expect(result.content[0]!.text).toContain('`includeVerbose` is valid only for views "installed", "resident" and "detail"');
  });
});
