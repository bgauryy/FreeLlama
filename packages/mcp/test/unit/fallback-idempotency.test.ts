import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({ run: vi.fn(), route: vi.fn(), handlers: new Map<string, (...args: any[]) => Promise<any>>() }));
vi.mock("../../src/serve-client.js", () => ({ runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: fixture.route }));

import { registerRunTaskTool } from "../../src/tools/run-task.js";
import { streamTarget } from "../../src/agent-cli/stream.js";

beforeAll(() => {
  registerRunTaskTool({ registerTool: (name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) => {
    fixture.handlers.set(name, handler);
  } } as unknown as McpServer);
});
beforeEach(() => {
  fixture.run.mockReset();
  fixture.run.mockImplementation(async () => JSON.stringify({ deferred: true, job: { id: crypto.randomUUID(), status: "queued" } }));
});

const extra = () => ({ signal: new AbortController().signal });
const text = (result: { content?: Array<{ text?: string }> }) => result.content?.map((part) => part.text ?? "").join("\n") ?? "";

describe("run_task fallback chains and idempotency keys", () => {
  it("sends the chain and the key to the core as fallback_models and idempotency_key", async () => {
    const handler = fixture.handlers.get("run_task")!;
    const result = await handler({ model: "small:1", fallbackModels: ["big:1"], prompt: "hi", defer: true, idempotencyKey: "nightly-1" }, extra());
    expect(result.isError).toBeFalsy();
    expect(fixture.run.mock.calls[0]![1]).toMatchObject({ model: "small:1", fallback_models: ["big:1"], idempotency_key: "nightly-1", defer: true });
  });

  it("shares a top-level chain with every batch item that sets none", async () => {
    const handler = fixture.handlers.get("run_task")!;
    await handler({ model: "small:1", fallbackModels: ["big:1"], defer: true, tasks: [
      { id: "a", independent: true, task: { prompt: "one" } },
      { id: "b", independent: true, task: { prompt: "two", fallbackModels: ["other:1"] } },
    ] }, extra());
    const sent = new Map(fixture.run.mock.calls.map(([, request]) => [request.prompt ?? request.messages?.at(-1)?.content, request]));
    expect(sent.get("one")?.fallback_models).toEqual(["big:1"]);
    expect(sent.get("two")?.fallback_models).toEqual(["other:1"]);
  });

  it("refuses a key without defer, and both fields in a routing preview", async () => {
    const handler = fixture.handlers.get("run_task")!;
    expect(text(await handler({ model: "m:1", prompt: "hi", idempotencyKey: "k" }, extra()))).toMatch(/pass it with `defer:true`/);
    const preview = await handler({ model: "m:1", preview: true, fallbackModels: ["n:1"] }, extra());
    expect(preview.isError).toBe(true);
    expect(text(preview)).toMatch(/`fallbackModels`/);
    expect(fixture.run).not.toHaveBeenCalled();
  });

  it("keeps a chain or a key on the managed path, never the direct stream that would drop it", () => {
    expect(streamTarget({ model: "small:1", prompt: "hi" })).toBeDefined();
    expect(streamTarget({ model: "small:1", prompt: "hi", fallbackModels: ["big:1"] })).toBeUndefined();
  });

  it("keeps one model per answer: no chain over separately sliced embeddings", async () => {
    const handler = fixture.handlers.get("run_task")!;
    const input = Array.from({ length: 200 }, (_, index) => `item ${index}`);
    const result = await handler({ task: "embedding", model: "e:1", fallbackModels: ["f:1"], input }, extra());
    expect(result.isError).toBe(true);
    expect(text(result)).toMatch(/one model per answer/);
    expect(fixture.run).not.toHaveBeenCalled();
  });
});
