import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

describe("deferred task lifecycle through native MCP", () => {
  const jobId = "b0bf3d83-4e2e-4bcb-a11e-a3c7a8a66d54";
  const requests: { method: string; path: string; body: any }[] = [];
  let server: Server;
  let client: Client;
  let endpoint: string;
  let embeddings = false;

  beforeAll(async () => {
    server = createServer(async (request, response) => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      requests.push({ method: request.method!, path: request.url!, body: raw ? JSON.parse(raw) : null });
      response.setHeader("content-type", "application/json");
      if (request.url?.endsWith("/tasks")) {
        response.statusCode = 202;
        response.end(JSON.stringify({ deferred: true, job: { id: jobId, status: "queued" } }));
      } else if (request.url?.endsWith("/jobs")) {
        response.end(JSON.stringify({ jobs: [{ id: jobId, status: "waiting_for_resources" }] }));
      } else if (request.method === "DELETE") {
        response.end(JSON.stringify({ id: jobId, removed: true, status: "cancelled", scope: "process_memory" }));
      } else {
        response.end(JSON.stringify({ job: { id: jobId, status: request.method === "POST" ? "cancelled" : "completed",
          result: embeddings ? { response: { embeddings: [[0.1, 0.2], [0.3, 0.4]] } } : { response: { message: { content: "Done" } } },
        } }));
      }
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing mock address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient();
  });
  beforeEach(() => { requests.length = 0; embeddings = false; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });

  it("submits caller controls, inspects waiting work, retrieves its result, and cancels only that ID", async () => {
    const submitted = await client.callTool({ name: "run_task", arguments: {
      endpoint, prompt: "Private", model: "installed:exact", contextTokens: 8192, defer: true,
      priority: "background", maxWaitSeconds: 20, timeoutSeconds: 30, options: { num_predict: 32 },
    } });
    expect(submitted.isError).not.toBe(true);
    expect(submitted.structuredContent).toEqual({ deferred: true, job: { id: jobId, status: "queued" } });
    expect(requests[0].body).toMatchObject({ model: "installed:exact", context_tokens: 8192,
      defer: true, priority: "background", max_wait_seconds: 20, timeout_seconds: 30,
      request_options: { options: { num_predict: 32 } },
    });
    for (const action of ["list", "get", "cancel", "remove"]) {
      const result = await client.callTool({ name: "task_jobs", arguments: {
        endpoint, action, ...(action === "list" ? {} : { jobId }),
      } });
      expect(result.isError).not.toBe(true);
      expect(result.structuredContent).toBeTruthy();
    }
    expect(requests.slice(1).map(({ method, path }) => [method, path])).toEqual([
      ["GET", "/_freellama/v1/jobs"], ["GET", `/_freellama/v1/jobs/${jobId}`],
      ["POST", `/_freellama/v1/jobs/${jobId}/cancel`],
      ["DELETE", `/_freellama/v1/jobs/${jobId}`],
    ]);
  });

  it.each([{ action: "get" }, { action: "cancel" }, { action: "remove" }, { action: "list", jobId },
    { action: "cancel", jobId, returnEmbeddings: false },
    { action: "remove", jobId, returnEmbeddings: false }, { action: "remove", jobId: "../another-job" },
  ])("rejects incomplete or incompatible job controls before HTTP: %j", async (args) => {
    const result = await client.callTool({ name: "task_jobs", arguments: { endpoint, ...args } });
    expect(result.isError).toBe(true);
    expect(requests).toHaveLength(0);
  });

  it.each([{ defer: true }, { defer: false }, { timeoutSeconds: 1 }, { priority: "interactive" }])("rejects execution controls in a preview: %j", async (controls) => {
    const result = await client.callTool({ name: "run_task", arguments: { endpoint, preview: true, ...controls } });
    expect(result.isError).toBe(true);
    expect(requests).toHaveLength(0);
  });

  it("summarizes retained embedding vectors unless explicitly requested", async () => {
    embeddings = true;
    const compact: any = await client.callTool({ name: "task_jobs", arguments: { endpoint, action: "get", jobId } });
    expect(compact.structuredContent.job.result.response.embeddings).toBeUndefined();
    expect(compact.structuredContent.job.result.response.embeddings_omitted).toMatchObject({ count: 2, dimensions: 2 });
    const full: any = await client.callTool({ name: "task_jobs", arguments: { endpoint, action: "get", jobId, returnEmbeddings: true } });
    expect(full.structuredContent.job.result.response.embeddings).toEqual([[0.1, 0.2], [0.3, 0.4]]);
  });
});
