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
  let refusal: Record<string, unknown> | undefined;
  let acknowledgeDeferred = false;
  beforeAll(async () => {
    server = createServer(async (request, response) => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      requests.push({ method: request.method!, path: request.url!, body: raw ? JSON.parse(raw) : undefined });
      response.setHeader("content-type", "application/json");
      if (refusal) { response.writeHead(404).end(JSON.stringify(refusal)); return; }
      if (acknowledgeDeferred && request.url?.includes("tasks") && requests.at(-1)!.body?.defer === true) {
        const item = requests.length;
        if (requests.at(-1)!.body?.prompt === "refused") { response.writeHead(429).end(JSON.stringify({ error: "deferred task registry is full", code: "task_jobs_full" })); return; }
        response.end(JSON.stringify({ deferred: true, job: { id: `00000000-0000-4000-8000-${String(item).padStart(12, "0")}`, status: "queued" } }));
        return;
      }
      if (request.method === "DELETE") { response.writeHead(204).end(); return; }
      response.end(JSON.stringify(request.url?.includes("tasks")
        ? { response: { message: { role: "assistant", content: "ok" }, done: true } }
        : { scope_id: id, revision: 0 }));
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing fixture address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient({ FREELLAMA_MCP_AUTOSTART_SERVE: "0" });
  });
  beforeEach(() => { requests.length = 0; refusal = undefined; acknowledgeDeferred = false; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });
  const call = (name: string, args: Record<string, unknown>) => client.callTool({ name, arguments: { endpoint, ...args } });

  it("creates a scope with only its messages; routing and limits are not MCP scope fields", async () => {
    const result = await call("scope", { action: "create", messages: [] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests[0]).toEqual({ method: "POST", path: "/_freellama/v1/scopes", body: { messages: [] } });
    for (const extra of [{ routeDefaults: { model: "test:latest" } }, { limits: { maxBytes: 50 } }]) {
      expect((await call("scope", { action: "create", messages: [], ...extra })).isError).toBe(true);
    }
    expect(requests).toHaveLength(1);
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
  it.each(["get", "delete"])("preserves scope %s refusal codes and details", async (action) => {
    refusal = { error: "scope missing", code: "scope_not_found", scope_id: id };
    const result = await call("scope", { action, scopeId: id });
    expect(result.isError).toBe(true);
    expect(result.structuredContent).toEqual(refusal);
  });
  it.each(["run_task", "batch"])("%s preserves scoped defaults and forwards explicit overrides", async (name) => {
    const task = { scopeId: id, scopeRevision: 3, prompt: "continue" };
    const result = await call("run_task", name === "run_task" ? task : { tasks: [{ id: "one", independent: true, task }] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const sent = requests[0].body;
    expect(sent).toMatchObject({ scope_id: id, scope_revision: 3, prompt: "continue" });
    for (const field of ["task", "objective", "model", "context_tokens", "required_capabilities", "execution_preference", "min_placement_evidence", "min_confidence"]) {
      expect(sent).not.toHaveProperty(field);
    }
    await call("run_task", name === "run_task" ? { ...task, task: "coding", objective: "fastest" }
      : { tasks: [{ id: "one", independent: true, task: { ...task, task: "coding", objective: "fastest" } }] });
    const override = requests[1].body;
    expect(override).toMatchObject({ task: "coding", objective: "fastest" });
  });
  it.each([
    { name: "scope", args: { action: "get", scopeId: id, messages: [] } },
    { name: "scope", args: { action: "fork", scopeId: id } },
    { name: "scope", args: { action: "delete", scopeId: id, includeMessages: false } },
    { name: "run_task", args: { scopeId: id, prompt: "missing revision" } },
    { name: "run_task", args: { preview: true, scopeId: id, scopeRevision: 0 } },
    { name: "run_task", args: { warm: true, model: "test:latest" } },
    { name: "run_task", args: { task: "code_review", prompt: "aliases are gone" } },
    { name: "ollama_manage", args: { action: "warm", model: "test:latest", ollamaEndpoint: "http://127.0.0.1:1" } },
    { name: "ollama_manage", args: { action: "warm", model: "d:1", task: "decision" } },
    { name: "run_task", args: { task: "decision", state: "s" } },
    { name: "run_task", args: { task: "decision", state: "s", questions: { q: { type: "noul", instructions: "?" } }, prompt: "p" } },
    { name: "run_task", args: { task: "completion", prompt: "p", questions: { q: { type: "noul", instructions: "?" } } } },
    { name: "run_task", args: { task: "completion", prompt: "p", nearest: { top: 3 } } },
    { name: "run_task", args: { prompt: "solo", maxParallelism: 2 } },
    { name: "run_task", args: { prompt: "shared prompt", tasks: [{ id: "a", independent: true, task: { prompt: "x" } }] } },
  ])("rejects action or preview misuse before contacting the server ($name)", async ({ name, args }) => {
    expect((await call(name, args)).isError).toBe(true);
    expect(requests).toHaveLength(0);
  });
  const batch = [{ id: "a", independent: true, task: { prompt: "x" } }];
  it.each([
    { args: { prompt: "p", tasks: batch }, message: "With `tasks`, put `prompt` inside each item's `task`; only `task`, `model`," },
    { args: { returnEmbeddings: true, preview: true, nearest: { top: 2 }, tasks: batch },
      message: "`returnEmbeddings`, `nearest`, `preview` are not supported with `tasks`." },
  ])("names the right fix for a stray batch field ($message)", async ({ args, message }) => {
    const result = await call("run_task", args);
    expect(result.isError).toBe(true);
    expect(JSON.stringify(result.content)).toContain(JSON.stringify(message).slice(1, -1));
    expect(requests).toHaveLength(0);
  });
  it("defer with tasks submits each item as its own deferred job and reports every job id", async () => {
    acknowledgeDeferred = true;
    const result = await call("run_task", { defer: true, model: "m:1", maxParallelism: 1, tasks: [
      { id: "a", independent: true, task: { prompt: "first" } },
      { id: "b", independent: true, task: { prompt: "refused" } },
      { id: "c", independent: true, task: { prompt: "third" } },
    ] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const jobs = (result.structuredContent as any).jobs;
    expect(result.structuredContent).toMatchObject({ deferred: true });
    expect(jobs.map((row: any) => row.id)).toEqual(["a", "b", "c"]);
    expect(jobs[0].job).toMatchObject({ status: "queued" });
    expect(jobs[1]).toMatchObject({ error: { code: "task_jobs_full" } });
    expect(jobs[2].job.id).not.toBe(jobs[0].job.id);
    expect(requests.map((r) => r.body)).toMatchObject([
      { model: "m:1", defer: true, prompt: "first" }, { defer: true, prompt: "refused" }, { defer: true, prompt: "third" },
    ]);
    expect(requests.every((r) => r.path === "/_freellama/v1/tasks")).toBe(true);
  });
  it("sends a nearest ranking request to serve instead of ranking locally, also with defer", async () => {
    acknowledgeDeferred = true;
    const result = await call("run_task", { task: "embedding", input: ["x", "y"], defer: true,
      nearest: { query: "q", top: 3, minScore: 0.2 }, returnEmbeddings: true });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(result.structuredContent).toMatchObject({ deferred: true });
    expect(requests[0]!.body).toMatchObject({
      task: "embedding", defer: true, input: ["x", "y"], return_embeddings: true,
      nearest: { query: "q", top: 3, min_score: 0.2, labels: ["input[0]", "input[1]"] },
    });
  });
  it("sends shared batch fields once as defaults that each item may override", async () => {
    const questions = { kind: { type: "choice", instructions: "Which kind?", criteria: { bug: "Broken", feature: "New" } } };
    const result = await call("run_task", { task: "decision", model: "d:1", questions, tasks: [
      { id: "a", independent: true, task: { state: "Crash on save" } },
      { id: "b", independent: true, task: { state: "Add dark mode", model: "d:2" } },
    ] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const a = requests.find((item) => item.body?.state === "Crash on save")!.body;
    const b = requests.find((item) => item.body?.state === "Add dark mode")!.body;
    expect(a).toMatchObject({ task: "decision", model: "d:1", questions, state: "Crash on save" });
    expect(b).toMatchObject({ task: "decision", model: "d:2", questions, state: "Add dark mode" });
  });
  it("sends a shared `tools` list to every item and lets an item's own list replace it", async () => {
    const shared = [{ type: "function", function: { name: "lookup", parameters: { type: "object" } } }];
    const own = [{ type: "function", function: { name: "other" } }];
    const result = await call("run_task", { model: "t:1", tools: shared, tasks: [
      { id: "a", independent: true, task: { prompt: "first" } },
      { id: "b", independent: true, task: { prompt: "second" } },
      { id: "c", independent: true, task: { prompt: "third", tools: own } },
    ] });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const body = (prompt: string) => requests.find((item) => item.body?.prompt === prompt)!.body;
    expect(body("first").tools).toEqual(shared);
    expect(body("second").tools).toEqual(shared);
    expect(body("third").tools).toEqual(own);
  });
  it("forwards warming controls with no prompt/history and no invented TTL", async () => {
    const result = await call("ollama_manage", { action: "warm", model: "test:latest", contextTokens: 4096, defer: true, priority: "background", timeoutSeconds: 10 });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests[0]).toEqual({ method: "POST", path: "/_freellama/v1/warm", body: {
      model: "test:latest", context_tokens: 4096, defer: true, priority: "background", timeout_seconds: 10,
    } });
  });
  it("forwards an embedding warm to the server, which loads it with an empty embed request", async () => {
    const result = await call("ollama_manage", { action: "warm", model: "test:latest", task: "embedding", keepAlive: "60s" });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    // Warm runs as a cancellable job so a cold load reports progress; serve gets defer:true.
    expect(requests[0]).toEqual({ method: "POST", path: "/_freellama/v1/warm", body: {
      model: "test:latest", task: "embedding", keep_alive: "60s", defer: true,
    } });
  });
  it.runIf(releaseServeAvailable)("CLI omitted controls preserve saved routing defaults", async () => {
    await promisify(execFile)(RELEASE_BINARY, ["task", "continue", "--endpoint", endpoint,
      "--scope-id", id, "--scope-revision", "3"]);
    expect(requests[0].body).toMatchObject({ scope_id: id, scope_revision: 3, prompt: "continue" });
    for (const field of ["task", "objective", "model", "context_tokens", "execution_preference", "min_placement_evidence", "min_confidence", "keep_alive"]) {
      expect(requests[0].body).not.toHaveProperty(field);
    }
  });
  it.runIf(releaseServeAvailable)("CLI scope and warm use public paths and controls", async () => {
    const exec = promisify(execFile);
    await exec(RELEASE_BINARY, ["scope", "create", "--endpoint", endpoint, "--json", '{"route_defaults":{"model":"test:latest"}}']);
    await exec(RELEASE_BINARY, ["warm", "--endpoint", endpoint, "--model", "test:latest", "--keep-alive", "2m", "--defer"]);
    await exec(RELEASE_BINARY, ["scope", "delete", "--endpoint", endpoint, "--scope-id", id]);
    expect(requests[0].body).toEqual({ route_defaults: { model: "test:latest" } });
    expect(requests[1]).toMatchObject({ path: "/_freellama/v1/warm", body: { model: "test:latest", keep_alive: "2m", defer: true } });
    expect(requests[2].method).toBe("DELETE");
  });
});
