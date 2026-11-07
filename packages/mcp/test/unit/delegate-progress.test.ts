// Lifecycle tracking is independent of progressToken; only human-facing notifications depend on it.
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { PROGRESS_ENV, PROGRESS_PREFIX } from "../../src/research/progress-events.js";

const fixture = vi.hoisted(() => ({
  handlers: new Map<string, (...args: any[]) => Promise<any>>(),
  exec: vi.fn(),
  cancelTaskJob: vi.fn(async (_endpoint: string, _id: string) => "{}"),
  result: {} as Record<string, unknown>,
}));
const DEFAULT_RESULT = { final_answer: "foo is in src/a.ts", tool_calls: [], usage: {} };

vi.mock("../../src/serve-client.js", () => ({ route: vi.fn(), cancelTaskJob: fixture.cancelTaskJob }));
vi.mock("node:fs", async (importOriginal) => {
  const original = await importOriginal<typeof import("node:fs")>();
  return { ...original, existsSync: (file: string) => /(?:bash|octocode)-agent\.js$/.test(file) || original.existsSync(file) };
});
vi.mock("node:fs/promises", async (importOriginal) => ({
  ...await importOriginal<typeof import("node:fs/promises")>(),
  mkdtemp: vi.fn(async () => "/mock-delegate"),
  writeFile: vi.fn(async () => undefined),
  readFile: vi.fn(async () => JSON.stringify(fixture.result)),
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
    fixture.cancelTaskJob.mockClear();
    fixture.result = DEFAULT_RESULT;
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

  it("cancels a turn's job that the adapter left open when it was killed", async () => {
    fixture.exec.mockImplementation(async (_file: string, _argv: string[], options: Options) => {
      options.stderrFilter!.push(Buffer.from(
        `${PROGRESS_PREFIX}{"event":"job_open","id":"done-job"}\n${PROGRESS_PREFIX}{"event":"job_closed","id":"done-job"}\n` +
          `${PROGRESS_PREFIX}{"event":"job_open","id":"live-job"}\n` +
          `${PROGRESS_PREFIX}{"event":"job_phase","turn":2,"maxTurns":8,"message":"loading m (~8.7 s)"}\n`,
      ));
      throw new Error("Command failed: killed by SIGKILL");
    });
    const sendNotification = vi.fn(async () => undefined);
    await fixture.handlers.get("delegate_research")!({ ...args, endpoint: "http://127.0.0.1:11435" }, {
      signal: new AbortController().signal, _meta: { progressToken: "t2" }, sendNotification,
    });
    expect(fixture.cancelTaskJob.mock.calls).toEqual([["http://127.0.0.1:11435", "live-job"]]);
    const sent = sendNotification.mock.calls.map(([notification]: any[]) => notification.params.message);
    expect(sent).toContain("turn 2/8: loading m (~8.7 s)");
  });

  it("tracks and cleans up jobs without a progressToken while keeping notifications quiet", async () => {
    vi.stubEnv(PROGRESS_ENV, "stderr");
    fixture.exec.mockImplementation(async (_file: string, _argv: string[], options: Options) => {
      expect(options.env[PROGRESS_ENV]).toBe("stderr");
      const kept = options.stderrFilter!.push(Buffer.from(
        `${PROGRESS_PREFIX}{"event":"job_open","id":"quiet-job"}\n` +
        `${PROGRESS_PREFIX}{"event":"model","turn":1,"maxTurns":8}\ndiagnostic\n`,
      ));
      expect(kept.toString()).toBe("diagnostic\n");
      return { stdout: "", stderr: "" };
    });
    const sendNotification = vi.fn(async () => undefined);
    const result = await fixture.handlers.get("delegate_research")!(args, { signal: new AbortController().signal, sendNotification });
    expect(result.isError).toBeFalsy();
    expect(fixture.exec).toHaveBeenCalledTimes(1);
    expect(fixture.cancelTaskJob).toHaveBeenCalledWith(expect.any(String), "quiet-job");
    expect(sendNotification).not.toHaveBeenCalled();
  });

  it("cites only steps that read something; failed and empty steps stay in the trail with their status", async () => {
    fixture.result = {
      ...DEFAULT_RESULT,
      tool_calls: [
        { raw_name: "shell", arguments: { command: "cat package.json" }, status: "ok", result: "{}" },
        { raw_name: "shell", arguments: { command: "cat missing.txt" }, status: "error", exit_code: 1, result: "No such file" },
        { raw_name: "shell", arguments: { command: "rg absent package.json" }, status: "empty", exit_code: 1, result: "nothing found" },
        { raw_name: "shell", arguments: { command: "cat package.json" }, status: "ok", result: "{}" },
      ],
    };
    fixture.exec.mockResolvedValue({ stdout: "", stderr: "" });
    const result = await fixture.handlers.get("delegate_research")!(args, { signal: new AbortController().signal });
    expect(result.isError).toBeFalsy();
    const content = result.structuredContent;
    expect(content.citations.map((citation: { step: number }) => citation.step)).toEqual([1, 4]);
    expect(content.toolCallCount).toBe(4);
    expect(content.successfulToolCallCount).toBe(2);
    expect(content.evidence.map((step: { status: string }) => step.status)).toEqual(["ok", "error", "empty", "ok"]);
    expect(content.evidence[1]).toMatchObject({ exit_code: 1 });
    expect(content.summary).toContain("2 of which did not succeed");
  });
});
