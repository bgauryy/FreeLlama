import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient, releaseServeAvailable, startIsolatedServe } from "../setup/client.js";

describe("session kill protocol", () => {
  let client: Client;
  let server: Server;
  let endpoint: string;
  const id = "00000000-0000-4000-8000-000000000001";
  const requests: string[] = [];
  beforeAll(async () => {
    server = createServer(async (request, response) => {
      for await (const _ of request) { /* Drain the request body. */ }
      requests.push(`${request.method} ${request.url}`);
      response.setHeader("content-type", "application/json");
      if (request.url?.endsWith(`/${id}/kill`)) {
        response.end(JSON.stringify({ session_id: id, killed: true, cancellation: "requested", runner_stop_confirmed: false, model_unloaded: false }));
      } else {
        response.statusCode = 404;
        response.end(JSON.stringify({ error: "session does not exist or has expired" }));
      }
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing mock address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient();
  });
  beforeEach(() => { requests.length = 0; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  });
  it("advertises kill and forwards the exact session to the native control plane", async () => {
    const tool = (await client.listTools()).tools.find((tool) => tool.name === "session")!;
    expect((tool.inputSchema.properties!.action as any).enum).toContain("kill");
    expect(tool.annotations?.destructiveHint).toBe(true);
    const result = await client.callTool({ name: "session", arguments: { endpoint, action: "kill", sessionId: id } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(result.structuredContent).toEqual({ session_id: id, killed: true, cancellation: "requested", runner_stop_confirmed: false, model_unloaded: false });
    expect(requests).toEqual([`POST /_freellama/v1/sessions/${id}/kill`]);
  });
  it("rejects missing and malformed IDs without a control request", async () => {
    for (const sessionId of [undefined, "../other-session"]) {
      const result = await client.callTool({ name: "session", arguments: { endpoint, action: "kill", sessionId } });
      expect(result.isError).toBe(true);
    }
    expect(requests).toHaveLength(0);
  });
  it("does not claim a missing or expired session was killed", async () => {
    const result = await client.callTool({ name: "session", arguments: { endpoint, action: "kill", sessionId: "00000000-0000-4000-8000-000000000002" } });
    expect(result.isError).toBe(true);
    expect(result.structuredContent).not.toMatchObject({ killed: true });
  });

  it.runIf(releaseServeAvailable)("creates, kills, refuses reuse, and deletes through a real isolated serve", async () => {
    const isolated = await startIsolatedServe();
    try {
      const call = (name: string, args: Record<string, unknown>) => client.callTool({ name, arguments: { endpoint: isolated.endpoint, ...args } });
      const created = await call("session", { action: "create" });
      expect(created.isError, JSON.stringify(created)).not.toBe(true);
      const sessionId = (created.structuredContent as Record<string, unknown>).session_id;
      const killed = await call("session", { action: "kill", sessionId });
      expect(killed.isError, JSON.stringify(killed)).not.toBe(true);
      expect(killed.structuredContent).toMatchObject({ session_id: sessionId, killed: true, runner_stop_confirmed: false });
      // Refusal precedes discovery/inference, so this test does not load any model.
      expect((await call("run_task", { sessionId, prompt: "must not execute" })).isError).toBe(true);
      expect((await call("session", { action: "kill", sessionId })).isError).toBe(true);
      const next = await call("session", { action: "create" });
      const nextId = (next.structuredContent as Record<string, unknown>).session_id;
      const deleted = await call("session", { action: "delete", sessionId: nextId });
      expect(deleted.isError, JSON.stringify(deleted)).not.toBe(true);
      expect(deleted.structuredContent).toMatchObject({ session_id: nextId, deleted: true });
    } finally {
      isolated.child.kill();
    }
  });
});
