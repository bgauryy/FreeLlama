import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient, RELEASE_BINARY, releaseServeAvailable } from "../setup/client.js";

const id = "b38c77a7-ccfe-41f3-a848-82daa7b27e63";

describe("scope and warming client contracts", () => {
  let server: Server;
  let endpoint: string;
  let client: Client;
  const requests: { method: string; path: string; body: any }[] = [];
  beforeAll(async () => {
    server = createServer(async (request, response) => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      requests.push({ method: request.method!, path: request.url!, body: raw ? JSON.parse(raw) : undefined });
      response.setHeader("content-type", "application/json");
      if (request.method === "DELETE") { response.writeHead(204).end(); return; }
      response.end(JSON.stringify(request.url?.includes("tasks")
        ? { response: { message: { role: "assistant", content: "ok" }, done: true } }
        : { scope_id: id, revision: 0 }));
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing fixture address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient({ FREELLAMA_AUTOSTART_SERVE: "0" });
  });
  beforeEach(() => { requests.length = 0; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });
  const call = (name: string, args: Record<string, unknown>) => client.callTool({ name, arguments: { endpoint, ...args } });

  it("maps explicit create defaults and limits without adding routing defaults", async () => {
    const result = await call("scope", { action: "create", messages: [],
      routeDefaults: { task: "code_review", model: "test:latest", contextTokens: 4096 },
      limits: { maxBytes: 10000, ttlSeconds: 60 } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests[0]).toEqual({ method: "POST", path: "/_freellama/v1/scopes", body: {
      messages: [], route_defaults: { task: "coding", model: "test:latest", context_tokens: 4096 },
      limits: { max_bytes: 10000, ttl_seconds: 60 },
    } });
  });
  it("retrieves metadata by default and requires explicit history retrieval", async () => {
    await call("scope", { action: "get", scopeId: id });
    await call("scope", { action: "get", scopeId: id, includeMessages: true });
    expect(requests.map((r) => r.path)).toEqual([
      `/_freellama/v1/scopes/${id}?include_messages=false`, `/_freellama/v1/scopes/${id}?include_messages=true`,
    ]);
  });
  it("forks a specific revision and deletes without requiring a JSON 204 body", async () => {
    await call("scope", { action: "fork", scopeId: id, revision: 7 });
    const removed = await call("scope", { action: "delete", scopeId: id });
    expect(requests[0].body).toEqual({ revision: 7 });
    expect(removed.structuredContent).toEqual({ deleted: true, scope_id: id });
  });
  it.each(["run_task", "run_task_batch"])("%s preserves scoped defaults and forwards explicit overrides", async (name) => {
    const task = { scopeId: id, scopeRevision: 3, prompt: "continue" };
    const result = await call(name, name === "run_task" ? task : { tasks: [{ id: "one", independent: true, task }] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const sent = name === "run_task" ? requests[0].body : requests[0].body.tasks[0].task;
    expect(sent).toMatchObject({ scope_id: id, scope_revision: 3, prompt: "continue" });
    for (const field of ["task", "objective", "model", "context_tokens", "required_capabilities", "execution_preference", "min_placement_evidence", "min_confidence"]) {
      expect(sent).not.toHaveProperty(field);
    }
    await call(name, name === "run_task" ? { ...task, task: "code_review", objective: "fastest" }
      : { tasks: [{ id: "one", independent: true, task: { ...task, task: "code_review", objective: "fastest" } }] });
    const override = name === "run_task" ? requests[1].body : requests[1].body.tasks[0].task;
    expect(override).toMatchObject({ task: "coding", objective: "fastest" });
  });
  it.each([
    { name: "scope", args: { action: "get", scopeId: id, limits: { maxBytes: 50 } } },
    { name: "scope", args: { action: "fork", scopeId: id } },
    { name: "scope", args: { action: "delete", scopeId: id, includeMessages: false } },
    { name: "run_task", args: { scopeId: id, prompt: "missing revision" } },
    { name: "run_task", args: { preview: true, scopeId: id, scopeRevision: 0 } },
    { name: "warm_model", args: { model: "test:latest", task: "embedding" } },
    { name: "warm_model", args: { model: "test:latest", prompt: "hidden generation" } },
  ])("rejects action or preview misuse before contacting the server ($name)", async ({ name, args }) => {
    expect((await call(name, args)).isError).toBe(true);
    expect(requests).toHaveLength(0);
  });
  it("forwards warming controls with no prompt/history and no invented TTL", async () => {
    const result = await call("warm_model", { model: "test:latest", contextTokens: 4096, defer: true, priority: "background", timeoutSeconds: 10 });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests[0]).toEqual({ method: "POST", path: "/_freellama/v1/warm", body: {
      model: "test:latest", context_tokens: 4096, defer: true, priority: "background", timeout_seconds: 10,
    } });
  });
  it.skipIf(!releaseServeAvailable)("CLI omitted controls preserve saved routing defaults", async () => {
    await promisify(execFile)(RELEASE_BINARY, ["task", "continue", "--endpoint", endpoint,
      "--scope-id", id, "--scope-revision", "3"]);
    expect(requests[0].body).toMatchObject({ scope_id: id, scope_revision: 3, prompt: "continue" });
    for (const field of ["task", "objective", "model", "context_tokens", "execution_preference", "min_placement_evidence", "min_confidence", "keep_alive"]) {
      expect(requests[0].body).not.toHaveProperty(field);
    }
  });
  it.skipIf(!releaseServeAvailable)("CLI scope and warm use public paths and controls", async () => {
    const exec = promisify(execFile);
    await exec(RELEASE_BINARY, ["scope", "create", "--endpoint", endpoint, "--json", '{"route_defaults":{"model":"test:latest"}}']);
    await exec(RELEASE_BINARY, ["warm", "--endpoint", endpoint, "--model", "test:latest", "--keep-alive", "2m", "--defer"]);
    await exec(RELEASE_BINARY, ["scope", "delete", "--endpoint", endpoint, "--scope-id", id]);
    expect(requests[0].body).toEqual({ route_defaults: { model: "test:latest" } });
    expect(requests[1]).toMatchObject({ path: "/_freellama/v1/warm", body: { model: "test:latest", keep_alive: "2m", defer: true } });
    expect(requests[2].method).toBe("DELETE");
  });
});
