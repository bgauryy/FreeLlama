import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

const fixture = vi.hoisted(() => ({
  handlers: new Map<string, (...args: any[]) => Promise<any>>(),
  outputs: new Map<string, { parse: (value: unknown) => unknown }>(),
  result: {} as Record<string, unknown>,
  exec: vi.fn(),
  removed: vi.fn(),
  route: vi.fn(),
  write: vi.fn(),
  adapterExists: true,
}));

// Only the delegate tool's own dependencies are replaced: the managed route call and the
// subprocess/filesystem surface. The tool registers on a minimal recording server.
vi.mock("../../src/serve-client.js", () => ({ route: fixture.route }));
vi.mock("node:fs", async (importOriginal) => {
  const original = await importOriginal<typeof import("node:fs")>();
  return { ...original, existsSync: (file: string) => /(?:bash|octocode)-agent\.js$/.test(file) ? fixture.adapterExists : original.existsSync(file) };
});
vi.mock("node:fs/promises", async (importOriginal) => ({
  ...await importOriginal<typeof import("node:fs/promises")>(),
  mkdtemp: vi.fn(async () => "/mock-delegate"),
  writeFile: fixture.write,
  readFile: vi.fn(async () => JSON.stringify(fixture.result)),
  rm: fixture.removed,
}));
vi.mock("../../src/lifecycle.js", () => ({
  liveDelegates: new Set(),
  killDelegateTree: vi.fn(),
  runDelegate: (...args: unknown[]) => {
    const running = fixture.exec(...args);
    return { child: running.child, done: running };
  },
}));

describe("delegate_research refusal forwarding", () => {
  beforeAll(async () => {
    const { registerDelegateResearchTool } = await import("../../src/tools/delegate-research.js");
    const recorder = {
      registerTool(name: string, config: { outputSchema: { parse: (value: unknown) => unknown } }, handler: (...args: any[]) => Promise<any>) {
        fixture.handlers.set(name, handler);
        fixture.outputs.set(name, config.outputSchema);
      },
    };
    registerDelegateResearchTool(recorder as unknown as McpServer);
  });
  beforeEach(() => {
    fixture.adapterExists = true;
    fixture.result = {};
    fixture.route.mockReset();
    fixture.exec.mockReset();
    fixture.write.mockReset();
    fixture.removed.mockReset();
  });

  it("keeps routed-model refusals valid against the output schema before spawning", async () => {
    const receipt = { error: "observed placement required", code: "placement_unverified", placement: { status: "unverified" } };
    fixture.route.mockRejectedValue(new Error(JSON.stringify(receipt)));
    const result = await fixture.handlers.get("delegate_research")!({
      question: "Find a fact", workspacePath: process.cwd(),
    }, { signal: new AbortController().signal });
    expect(result.isError).toBe(true);
    expect(fixture.outputs.get("delegate_research")!.parse(result.structuredContent)).toEqual(result.structuredContent);
    expect(result.structuredContent).toMatchObject({ ...receipt, answer: "", verification: { recommendation: "escalate" } });
    expect(result.content[0].text).toContain(receipt.error);
    expect(fixture.route).toHaveBeenCalledTimes(1);
    expect(fixture.exec).not.toHaveBeenCalled();
  });

  it.each(["workspace", "missing-adapter", "unreadable-result", "plain-adapter-error", "terminal-error"])(
    "keeps %s failures valid against the output schema", async (phase) => {
      fixture.exec.mockImplementation(() => Object.assign(Promise.reject(new Error("adapter process failed")), {
        child: { kill: vi.fn() },
      }));
      fixture.adapterExists = phase !== "missing-adapter";
      if (phase === "plain-adapter-error") fixture.result = { final_answer: "adapter failed to parse model output" };
      if (phase === "terminal-error") fixture.write.mockRejectedValue(new Error("prompt file write failed"));
      const result = await fixture.handlers.get("delegate_research")!({
        question: "Find a fact", workspacePath: phase === "workspace" ? "/nonexistent-delegate-workspace" : process.cwd(),
        model: "unmeasured-fixture:latest",
      }, { signal: new AbortController().signal });
      expect(result.isError).toBe(true);
      expect(fixture.outputs.get("delegate_research")!.parse(result.structuredContent)).toEqual(result.structuredContent);
      expect(result.structuredContent.answer).toBe("");
      expect(result.structuredContent.summary).toBe(result.content[0].text);
      expect(result.structuredContent.verification.recommendation).toBe("escalate");
      expect(result.structuredContent.error).toBeTruthy();
      if (phase !== "workspace") expect(fixture.removed).toHaveBeenCalledWith("/mock-delegate", { recursive: true, force: true });
    },
  );

  it("forwards the managed receipt as an MCP error and still removes temporary files", async () => {
    const receipt = {
      error: "physical placement is not verified: configured=gpu, observed=unknown",
      code: "placement_unverified",
      reason: "observed evidence required",
      retry_after_seconds: 5,
      resource_admission: { status: "held", required_available_bytes: 4096 },
      lifecycle: { requested: "immediate_unload", status: "verified" },
      placement: { status: "unverified", processor: "unknown" },
    };
    fixture.result = {
      final_answer: `agent response failed: HTTP 422: ${receipt.error}`,
      tool_calls: [],
      model_metadata: { transport_error: { status: 422, receipt } },
    };
    fixture.exec.mockImplementation(() => Object.assign(Promise.reject(new Error("adapter exited 1")), {
      child: { kill: vi.fn() },
    }));
    const handler = fixture.handlers.get("delegate_research")!;
    const result = await handler({
      question: "Find a fact", workspacePath: process.cwd(), model: "unmeasured-fixture:latest",
    }, { signal: new AbortController().signal });
    expect(result.isError).toBe(true);
    expect(fixture.outputs.get("delegate_research")!.parse(result.structuredContent)).toEqual(result.structuredContent);
    expect(result.structuredContent).toMatchObject(receipt);
    expect(result.structuredContent.answer).toBe("");
    expect(result.structuredContent.summary).toContain(receipt.error);
    expect(result.structuredContent.verification).toMatchObject({ recommendation: "escalate", grounded: false });
    expect(result.content[0].text).toContain(receipt.error);
    expect(result.content[0].text).not.toContain("Command failed");
    expect(fixture.exec).toHaveBeenCalledTimes(1);
    expect(fixture.removed).toHaveBeenCalledWith("/mock-delegate", { recursive: true, force: true });
  });
});
