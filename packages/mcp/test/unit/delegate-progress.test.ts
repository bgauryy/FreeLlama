// delegate_research with and without a client progressToken: the adapter is asked for progress only
// when the client can receive it, and its progress lines become notifications/progress.
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { PROGRESS_ENV, PROGRESS_PREFIX } from "../../src/research/progress-events.js";

const fixture = vi.hoisted(() => ({
  handlers: new Map<string, (...args: any[]) => Promise<any>>(),
  exec: vi.fn(),
}));

vi.mock("../../src/serve-client.js", () => ({ route: vi.fn() }));
vi.mock("node:fs", async (importOriginal) => {
  const original = await importOriginal<typeof import("node:fs")>();
  return { ...original, existsSync: (file: string) => /(?:bash|octocode)-agent\.js$/.test(file) || original.existsSync(file) };
});
vi.mock("node:fs/promises", async (importOriginal) => ({
  ...await importOriginal<typeof import("node:fs/promises")>(),
  mkdtemp: vi.fn(async () => "/mock-delegate"),
  writeFile: vi.fn(async () => undefined),
  readFile: vi.fn(async () => JSON.stringify({ final_answer: "foo is in src/a.ts", tool_calls: [], usage: {} })),
  rm: vi.fn(async () => undefined),
}));
vi.mock("../../src/lifecycle.js", () => ({
  liveDelegates: new Set(),
  killDelegateTree: vi.fn(),
  runDelegate: (...args: unknown[]) => {
    const running = fixture.exec(...args);
    return { child: {}, done: running };
  },
}));

type Options = { env: NodeJS.ProcessEnv; stderrFilter?: { push(chunk: Buffer): Buffer; end(): Buffer } };

describe("delegate_research progress", () => {
  beforeAll(async () => {
    const { registerDelegateResearchTool } = await import("../../src/tools/delegate-research.js");
    const recorder = {
      registerTool(name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) {
        fixture.handlers.set(name, handler);
      },
    };
    registerDelegateResearchTool(recorder as unknown as McpServer);
  });
  beforeEach(() => {
    fixture.exec.mockReset();
    vi.unstubAllEnvs();
  });

  const args = { question: "Where is foo?", workspacePath: process.cwd(), model: "unmeasured-fixture:latest" };

  it("asks the adapter for progress and reports each step under the client's token", async () => {
    let filtered = "";
    fixture.exec.mockImplementation(async (_file: string, _argv: string[], options: Options) => {
      expect(options.env[PROGRESS_ENV]).toBe("stderr");
      filtered += options.stderrFilter!.push(Buffer.from(
        `${PROGRESS_PREFIX}{"event":"model","turn":1,"maxTurns":8}\nwarning\n` +
          `${PROGRESS_PREFIX}{"event":"action","turn":1,"maxTurns":8,"action":{"action":"shell","command":"grep -rn foo src"}}\n`,
      )).toString();
      filtered += options.stderrFilter!.end().toString();
      return { stdout: "", stderr: filtered };
    });
    const sendNotification = vi.fn(async () => undefined);
    const result = await fixture.handlers.get("delegate_research")!(args, {
      signal: new AbortController().signal, _meta: { progressToken: "t1" }, sendNotification,
    });
    expect(result.isError).toBeFalsy();
    expect(result.structuredContent.answer).toBe("foo is in src/a.ts");
    expect(filtered).toBe("warning\n");
    const sent = sendNotification.mock.calls.map(([notification]: any[]) => notification.params);
    expect(sent).toEqual([
      { progressToken: "t1", progress: 1, message: "starting bash research with unmeasured-fixture:latest" },
      { progressToken: "t1", progress: 2, message: "turn 1/8: model thinking" },
      { progressToken: "t1", progress: 3, message: "turn 1/8: grep 'foo' in src" },
    ]);
  });

  it("without a progressToken spawns exactly as before, even if the server env names the variable", async () => {
    vi.stubEnv(PROGRESS_ENV, "stderr");
    fixture.exec.mockImplementation(async (_file: string, _argv: string[], options: Options) => {
      expect(options.env[PROGRESS_ENV]).toBeUndefined();
      expect(options).not.toHaveProperty("stderrFilter");
      return { stdout: "", stderr: "" };
    });
    const sendNotification = vi.fn(async () => undefined);
    const result = await fixture.handlers.get("delegate_research")!(args, { signal: new AbortController().signal, sendNotification });
    expect(result.isError).toBeFalsy();
    expect(fixture.exec).toHaveBeenCalledTimes(1);
    expect(sendNotification).not.toHaveBeenCalled();
  });
});
