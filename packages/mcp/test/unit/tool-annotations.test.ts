// Tool annotations tell a client which calls need confirmation, and doctor's serve endpoint is
// named `endpoint` like every other tool. The native/serve boundary is mocked.
import { describe, expect, it, vi } from "vitest";

const status = vi.hoisted(() => vi.fn(async (_endpoint?: string) => JSON.stringify({ status: "ok" })));
vi.mock("../../src/native.js", () => ({ status, usage: vi.fn(), doctor: vi.fn(), health: vi.fn(), machine: vi.fn() }));
vi.mock("../../src/serve-client.js", () => ({}));

import { registerDoctorTool } from "../../src/tools/doctor.js";
import { registerModelsTool } from "../../src/tools/models.js";
import { registerTaskJobsTool } from "../../src/tools/task-jobs.js";
import { registerSessionTool } from "../../src/tools/session.js";
import { registerScopeTool } from "../../src/tools/scope.js";
import { registerDelegateResearchTool } from "../../src/tools/delegate-research.js";

type Handler = (args: Record<string, unknown>, extra?: unknown) => Promise<any>;
const tools = new Map<string, { annotations?: Record<string, unknown>; handler: Handler }>();
const recorder = { registerTool: (name: string, config: { annotations?: Record<string, unknown> }, handler: Handler) => {
  tools.set(name, { annotations: config.annotations, handler });
} } as never;
for (const register of [registerDoctorTool, registerModelsTool, registerTaskJobsTool, registerSessionTool, registerScopeTool, registerDelegateResearchTool]) {
  register(recorder);
}

describe("tool annotations", () => {
  it("marks only the caller's own jobs, sessions and scopes as changed, not destroyed", () => {
    for (const name of ["task_jobs", "session", "scope"]) {
      expect(tools.get(name)!.annotations, name).toEqual({ destructiveHint: false });
    }
  });

  it("leaves the tools that reach the internet open-world (the spec default), never closed", () => {
    expect(tools.get("models")!.annotations).toEqual({ readOnlyHint: true });
    expect(tools.get("delegate_research")!.annotations).toEqual({ destructiveHint: false });
  });
});

describe("doctor endpoint", () => {
  it("accepts endpoint, keeps serveEndpoint as an alias, and refuses two different values", async () => {
    const doctor = tools.get("doctor")!.handler;
    await doctor({ view: "status", endpoint: "http://127.0.0.1:1" });
    expect(status).toHaveBeenLastCalledWith("http://127.0.0.1:1");
    await doctor({ view: "status", serveEndpoint: "http://127.0.0.1:2" });
    expect(status).toHaveBeenLastCalledWith("http://127.0.0.1:2");
    const conflict = await doctor({ view: "status", endpoint: "http://127.0.0.1:1", serveEndpoint: "http://127.0.0.1:2" });
    expect(conflict.isError).toBe(true);
    expect(status).toHaveBeenCalledTimes(2);
  });
});
