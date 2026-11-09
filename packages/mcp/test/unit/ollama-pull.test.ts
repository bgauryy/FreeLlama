import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { registerOllamaLifecycleTools, type PullDependencies } from "../../src/tools/ollama-lifecycle.js";

type Handler = (args: Record<string, unknown>, extra: unknown) => Promise<any>;

const deps = {
  lookupTagSize: vi.fn<PullDependencies["lookupTagSize"]>(),
  installed: vi.fn<PullDependencies["installed"]>(),
  pull: vi.fn<PullDependencies["pull"]>(),
};
const handlers = new Map<string, Handler>();
const configs = new Map<string, { annotations?: Record<string, unknown> }>();
registerOllamaLifecycleTools({
  registerTool: (name: string, config: { annotations?: Record<string, unknown> }, handler: Handler) => {
    handlers.set(name, handler);
    configs.set(name, config);
  },
} as unknown as McpServer, deps);

const extra = { signal: new AbortController().signal, sendNotification: vi.fn(async () => undefined) };
const pull = (args: Record<string, unknown>) => handlers.get("ollama_pull")!(args, extra);
const text = (result: { content: { text: string }[] }) => result.content[0]!.text;

beforeEach(() => {
  deps.lookupTagSize.mockReset();
  deps.installed.mockReset();
  deps.pull.mockReset();
});

describe("ollama_pull", () => {
  it("is additive and idempotent, and open-world by the spec default", () => {
    expect(configs.get("ollama_pull")!.annotations).toEqual({ destructiveHint: false, idempotentHint: true });
    expect(configs.get("ollama_manage")!.annotations).toEqual({ destructiveHint: false });
  });

  it("refuses a bare family name before any lookup", async () => {
    const result = await pull({ model: "gemma3", approvedSizeBytes: 815_000_000 });
    expect(result.isError).toBe(true);
    expect(text(result)).toMatch(/exact tag such as gemma3:<tag>/);
    expect(deps.lookupTagSize).not.toHaveBeenCalled();
  });

  it("pulls when the reported size is within 5% of the approved size", async () => {
    deps.lookupTagSize.mockResolvedValue({ found: true, tag: "gemma3:1b", sizeBytes: 815_000_000, sourcePage: "https://ollama.com/library/gemma3" });
    deps.pull.mockResolvedValue({ status: "success", progress: { percent: 100 } });
    const result = await pull({ model: "gemma3:1b", approvedSizeBytes: 800_000_000, timeoutSeconds: 60 });
    expect(result.isError).toBeUndefined();
    expect(result.structuredContent).toMatchObject({ status: "success", approvedSizeBytes: 800_000_000, reportedSizeBytes: 815_000_000 });
    expect(deps.pull).toHaveBeenCalledWith(undefined, "gemma3:1b", 60, expect.objectContaining({ signal: extra.signal }));
  });

  it("refuses a size more than 5% from the approval, naming both numbers", async () => {
    deps.lookupTagSize.mockResolvedValue({ found: true, tag: "gemma3:1b", sizeBytes: 815_000_000, sourcePage: "https://ollama.com/library/gemma3" });
    const result = await pull({ model: "gemma3:1b", approvedSizeBytes: 700_000_000 });
    expect(result.isError).toBe(true);
    expect(text(result)).toMatch(/815000000 bytes.*700000000 bytes.*Re-ask the user/);
    expect(deps.pull).not.toHaveBeenCalled();
  });

  it("refuses a tag the library does not list, or one without a reported size", async () => {
    deps.lookupTagSize.mockResolvedValue({ found: false, tag: "gemma3:9z", sourcePage: "https://ollama.com/library/gemma3", message: "gemma3 lists no tag gemma3:9z." });
    expect(text(await pull({ model: "gemma3:9z", approvedSizeBytes: 1 }))).toMatch(/lists no tag gemma3:9z.*re-ask the user/);
    deps.lookupTagSize.mockResolvedValue({ found: true, tag: "x:cloud", sizeBytes: null, sourcePage: "https://ollama.com/library/x" });
    expect(text(await pull({ model: "x:cloud", approvedSizeBytes: 1 }))).toMatch(/no reported download size/);
    expect(deps.pull).not.toHaveBeenCalled();
  });

  it("reports an installed tag when the size lookup fails, and refuses anything else", async () => {
    deps.lookupTagSize.mockRejectedValue(new Error("fetch failed"));
    deps.installed.mockResolvedValue([{ name: "gemma3:latest", digest: "abc" }]);
    const installed = await pull({ model: "library/gemma3:latest", approvedSizeBytes: 1 });
    expect(installed.isError).toBeUndefined();
    expect(installed.structuredContent).toMatchObject({ status: "already_installed", pulled: false });
    expect(text(installed)).toMatch(/already installed; not pulled/);
    const missing = await pull({ model: "gemma3:1b", approvedSizeBytes: 1 });
    expect(missing.isError).toBe(true);
    expect(text(missing)).toMatch(/could not be looked up \(fetch failed\)/);
    deps.installed.mockRejectedValue(new Error("ollama down"));
    expect(text(await pull({ model: "gemma3:1b", approvedSizeBytes: 1 }))).toMatch(/local inventory could not be read/);
    expect(deps.pull).not.toHaveBeenCalled();
  });

  it("reports a streamed pull error as an error", async () => {
    deps.lookupTagSize.mockResolvedValue({ found: true, tag: "gemma3:1b", sizeBytes: 815_000_000, sourcePage: "" });
    deps.pull.mockResolvedValue({ status: "error", error: "manifest not found" });
    const result = await pull({ model: "gemma3:1b", approvedSizeBytes: 815_000_000 });
    expect(result.isError).toBe(true);
    expect(text(result)).toMatch(/pull gemma3:1b failed: manifest not found/);
  });
});

describe("ollama_manage pull alias", () => {
  it("downloads nothing and points to ollama_pull", async () => {
    const result = await handlers.get("ollama_manage")!({ action: "pull", model: "gemma3:1b" }, extra);
    expect(result.isError).toBe(true);
    expect(text(result)).toMatch(/no longer downloads.*ollama_pull \{model.*approvedSizeBytes/);
    expect(deps.pull).not.toHaveBeenCalled();
    expect(deps.lookupTagSize).not.toHaveBeenCalled();
  });
});
