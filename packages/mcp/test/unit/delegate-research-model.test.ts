import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

const STRONGER = "hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M";
const DEFAULT = "bonsai-2:9b";
const SMALL = "lfm3:1.7b-q4_K_M";
const GB = 1e9;

const fixture = vi.hoisted(() => ({
  handler: undefined as undefined | ((...args: any[]) => Promise<any>),
  exec: vi.fn(),
  route: vi.fn(),
  liveState: vi.fn(),
}));

// Fake boundaries only: the serve client, the live catalog state (inventory + free memory), and the subprocess.
vi.mock("../../src/serve-client.js", () => ({ route: fixture.route, listModels: vi.fn(async () => JSON.stringify({ models: [] })) }));
vi.mock("../../src/model-library.js", () => ({ catalogLiveState: fixture.liveState }));
vi.mock("node:fs", async (importOriginal) => {
  const original = await importOriginal<typeof import("node:fs")>();
  return { ...original, existsSync: (file: string) => /(?:bash|octocode|github)-agent\.js$/.test(file) ? true : original.existsSync(file) };
});
vi.mock("node:fs/promises", async (importOriginal) => ({
  ...await importOriginal<typeof import("node:fs/promises")>(),
  mkdtemp: vi.fn(async () => "/mock-delegate"),
  writeFile: vi.fn(),
  readFile: vi.fn(async () => JSON.stringify({ final_answer: "answer", tool_calls: [] })),
  rm: vi.fn(),
}));
vi.mock("../../src/lifecycle.js", () => ({
  liveDelegates: new Set(),
  killDelegateTree: vi.fn(),
  runDelegate: (...args: unknown[]) => {
    fixture.exec(...args);
    return { child: { kill: vi.fn() }, done: Promise.resolve() };
  },
}));

async function run(args: Record<string, unknown> = {}) {
  const result = await fixture.handler!({ question: "Where is X?", workspacePath: process.cwd(), ...args }, { signal: new AbortController().signal });
  expect(result.isError).toBeFalsy();
  return { model: fixture.exec.mock.calls[0]![2].env.FREELLAMA_TARGET_MODEL as string, text: result.content[0].text as string, result };
}

describe("delegate_research default model", () => {
  beforeAll(async () => {
    const { registerDelegateResearchTool } = await import("../../src/tools/delegate-research.js");
    registerDelegateResearchTool({
      registerTool(_name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) { fixture.handler = handler; },
    } as unknown as McpServer);
  });
  beforeEach(() => {
    fixture.exec.mockReset();
    fixture.route.mockReset();
    fixture.liveState.mockReset();
    fixture.route.mockResolvedValue(JSON.stringify({ selected_model: "routed-coder:latest" }));
  });

  it("chooses the stronger pick when it is installed and fits", async () => {
    fixture.liveState.mockResolvedValue([[{ name: STRONGER }, { name: DEFAULT }], 20 * GB]);
    const { model, text, result } = await run();
    expect(model).toBe(STRONGER);
    expect(text).toContain(`research model: ${STRONGER} (catalog stronger pick, installed, fits now)`);
    expect(result.structuredContent.researchModel).toEqual({ model: STRONGER, reason: "catalog stronger pick, installed, fits now" });
    expect(fixture.route).not.toHaveBeenCalled();
  });

  it("chooses the default pick when the stronger one does not fit", async () => {
    fixture.liveState.mockResolvedValue([[{ name: STRONGER }, { name: DEFAULT }], 11 * GB]);
    const { model, text } = await run();
    expect(model).toBe(DEFAULT);
    expect(text).toContain("catalog default pick, installed, fits now");
  });

  it("uses the small pick for local research when it is the only one that fits", async () => {
    fixture.liveState.mockResolvedValue([[{ name: STRONGER }, { name: DEFAULT }, { name: SMALL }], 4 * GB]);
    const { model, text } = await run();
    expect(model).toBe(SMALL);
    expect(text).toContain("catalog small pick, installed, fits now");
  });

  it("never defaults GitHub research to the small pick: the installed default goes to the memory gate", async () => {
    fixture.liveState.mockResolvedValue([[{ name: STRONGER }, { name: DEFAULT }, { name: SMALL }], 4 * GB]);
    const { model, text } = await run({ adapter: "github" });
    expect(model).toBe(DEFAULT);
    expect(text).toContain("catalog default pick, installed; not fitting now, so serve's memory gate decides");
    expect(fixture.route).not.toHaveBeenCalled();
  });

  it("falls back to routing when no pick is installed", async () => {
    fixture.liveState.mockResolvedValue([[{ name: "other:latest" }], 64 * GB]);
    const { model, text } = await run();
    expect(model).toBe("routed-coder:latest");
    expect(text).toContain("research model: routed-coder:latest (routing");
    expect(fixture.route).toHaveBeenCalledTimes(1);
  });

  it("falls back to routing when the catalog state cannot be read", async () => {
    fixture.liveState.mockRejectedValue(new Error("serve down"));
    expect((await run()).model).toBe("routed-coder:latest");
  });

  it("lets an explicit model win without consulting the catalog", async () => {
    fixture.liveState.mockResolvedValue([[{ name: STRONGER }], 64 * GB]);
    const { model, text } = await run({ model: "unmeasured-fixture:latest" });
    expect(model).toBe("unmeasured-fixture:latest");
    expect(text).toContain("(explicit model)");
    expect(fixture.liveState).not.toHaveBeenCalled();
    expect(fixture.route).not.toHaveBeenCalled();
  });
});
