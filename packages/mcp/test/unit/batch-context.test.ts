import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({ run: vi.fn(), handlers: new Map<string, (...args: any[]) => Promise<any>>() }));
vi.mock("../../src/serve-client.js", () => ({ runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn() }));

import { registerRunTaskTool, shareBatchContext } from "../../src/tools/run-task.js";

beforeAll(() => {
  registerRunTaskTool({ registerTool: (name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) => {
    fixture.handlers.set(name, handler);
  } } as unknown as McpServer);
});
beforeEach(() => {
  fixture.run.mockReset();
  fixture.run.mockImplementation(async () => JSON.stringify({ deferred: true, job: { id: crypto.randomUUID(), status: "queued" } }));
});

const item = (id: string, task: Record<string, unknown>) => ({ id, independent: true as const, task });

describe("batch context sizing", () => {
  it("gives every item of one model the widest explicit context", () => {
    const shared = shareBatchContext([
      item("a", { model: "m:1", prompt: "x" }),
      item("b", { model: "m:1", prompt: "y", contextTokens: 8192 }),
      item("c", { model: "m:1", prompt: "z", contextTokens: 4096 }),
      item("d", { model: "other:1", prompt: "w" }),
      item("e", { prompt: "routed" }),
      item("f", { model: "m:1", scopeId: "00000000-0000-4000-8000-000000000000", scopeRevision: 0, prompt: "s" }),
    ]);
    expect(shared.map((entry) => entry.task.contextTokens)).toEqual([8192, 8192, 8192, undefined, undefined, undefined]);
  });

  it("leaves a batch where no item of a model sets contextTokens to the core", () => {
    const tasks = [item("a", { model: "m:1", prompt: "x" }), item("b", { model: "m:1", prompt: "a much longer prompt" })];
    expect(shareBatchContext(tasks)).toEqual(tasks);
  });

  it("sends the shared window with every item of that model, and leaves single calls alone", async () => {
    const handler = fixture.handlers.get("run_task")!;
    const extra = { signal: new AbortController().signal };
    const batch = await handler({ model: "m:1", defer: true, tasks: [
      { id: "a", independent: true, task: { prompt: "short" } },
      { id: "b", independent: true, task: { prompt: "long", contextTokens: 16384 } },
      { id: "c", independent: true, task: { prompt: "other", model: "n:1" } },
    ] }, extra);
    expect(batch.isError).toBeFalsy();
    const sent = new Map(fixture.run.mock.calls.map(([, request]) => [request.prompt ?? request.messages?.at(-1)?.content, request]));
    expect(sent.get("short")).toMatchObject({ model: "m:1", context_tokens: 16384 });
    expect(sent.get("long")).toMatchObject({ model: "m:1", context_tokens: 16384 });
    expect(sent.get("other")?.context_tokens).toBeUndefined();

    fixture.run.mockClear();
    await handler({ model: "m:1", prompt: "single", defer: true }, extra);
    expect(fixture.run.mock.calls[0]![1].context_tokens).toBeUndefined();
  });
});
