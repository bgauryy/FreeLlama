import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

describe("caller-owned task prompts", () => {
  let client: Client;
  let server: Server;
  let endpoint: string;
  const requests: { path: string; body: any }[] = [];
  let refusal: { status: number; body: Record<string, unknown> } | undefined;

  beforeAll(async () => {
    server = createServer(async (request, response) => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      requests.push({ path: request.url!, body: JSON.parse(raw) });
      response.setHeader("content-type", "application/json");
      if (refusal) {
        response.statusCode = refusal.status;
        response.end(JSON.stringify(refusal.body));
        return;
      }
      response.end(JSON.stringify(request.url?.endsWith("task-batches")
        ? { results: [{ id: "review", ok: true, response: { message: { content: "Caller-defined prose." }, done: true } }] }
        : { response: { message: { content: "Caller-defined prose." }, done: true } }));
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing mock address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient();
  });
  beforeEach(() => { requests.length = 0; refusal = undefined; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });

  it("runs ordinary chat without a task label or workflow setup", async () => {
    const result = await client.callTool({ name: "run_task", arguments: { endpoint, prompt: "Hello" } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    expect(requests[0].body).toMatchObject({ task: "completion", objective: "balanced", prompt: "Hello", messages: [] });
    const tools = (await client.listTools()).tools;
    expect(tools.find((tool) => tool.name === "run_task")?.inputSchema.required ?? []).not.toContain("task");
  });

  it.each(["run_task", "run_task_batch"])("%s forwards a bounded caller waiting budget", async (name) => {
    const task = { prompt: "Hello", maxWaitSeconds: 30, timeoutSeconds: 60 };
    const result = await client.callTool({ name, arguments: name === "run_task"
      ? { endpoint, ...task }
      : { endpoint, tasks: [{ id: "review", independent: true, task }] },
    });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const sent = name === "run_task" ? requests[0].body : requests[0].body.tasks[0].task;
    expect(sent.max_wait_seconds).toBe(30);
    expect(sent.timeout_seconds).toBe(60);
  });

  it("rejects an execution wait budget in a routing-only preview", async () => {
    const result = await client.callTool({ name: "run_task", arguments: {
      endpoint, preview: true, maxWaitSeconds: 30,
    } });
    expect(result.isError).toBe(true);
    expect(requests).toHaveLength(0);
  });

  it.each([
    { status: 429, body: { error: "queue full", code: "admission_queue_full", retry_after_seconds: 7 } },
    { status: 503, body: { error: "capacity unavailable", code: "resource_admission_unavailable",
      retry_after_seconds: 3, resource_admission: { status: "deadline_exceeded", waited_ms: 1000 } } },
    { status: 502, body: { error: "runner disconnected", code: "upstream_transport_error",
      lifecycle: { requested: "immediate_unload", status: "verified" } } },
  ])("preserves the complete HTTP $status refusal receipt through native MCP", async (fixture) => {
    refusal = fixture;
    for (const name of ["run_task", "run_task_batch"]) {
      const result = await client.callTool({ name, arguments: name === "run_task"
        ? { endpoint, prompt: "Hello" }
        : { endpoint, tasks: [{ id: "review", independent: true, task: { prompt: "Hello" } }] },
      });
      expect(result.isError).toBe(true);
      expect(JSON.parse((result.content as { text: string }[])[0].text)).toEqual(fixture.body);
      expect(result.structuredContent).toEqual(fixture.body);
    }
  });

  it("advertises local-model prompts and typed function descriptors", async () => {
    const tools = (await client.listTools()).tools;
    for (const name of ["run_task", "run_task_batch"]) {
      const schema: any = tools.find((tool) => tool.name === name)!.inputSchema;
      const fields = name === "run_task" ? schema.properties : schema.properties.tasks.items.properties.task.properties;
      expect(fields.systemPrompt).toMatchObject({ type: "string" });
      expect(fields.systemPrompt.description).toMatch(/local model/i);
      expect(fields.tools.items.properties.function.properties).toHaveProperty("name");
      expect(fields.tools.items.properties.function.properties).toHaveProperty("description");
      expect(fields.tools.items.properties.function.properties).toHaveProperty("parameters");
    }
  });

  it.each(["run_task", "run_task_batch"])("%s forwards the explicit system prompt and tool-call conversation", async (name) => {
    const systemPrompt = "  Caller instructions for the local model.\n";
    const messages = [
      { role: "system", content: "Existing caller system message" },
      { role: "assistant", tool_calls: [{ function: { name: "lookup", arguments: { key: "x" } } }] },
      { role: "tool", tool_name: "lookup", content: "Found" },
    ];
    const tools = [{ type: "function", function: { name: "lookup", description: "Look up a key", parameters: {
      type: "object", properties: { key: { type: "string" } }, required: ["key"], additionalProperties: false,
    }, "x-caller-hint": "keep" } }];
    const task = { systemPrompt, messages, tools };
    const result = await client.callTool({ name, arguments: name === "run_task"
      ? { endpoint, ...task }
      : { endpoint, tasks: [{ id: "review", independent: true, task }] },
    });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    const sent = name === "run_task" ? requests[0].body : requests[0].body.tasks[0].task;
    expect(sent.messages).toEqual([{ role: "system", content: systemPrompt }, ...messages]);
    expect(sent.tools).toEqual(tools);
  });

  it("builds an explicit system/user pair without rewriting prompt or images", async () => {
    const result = await client.callTool({ name: "run_task", arguments: {
      endpoint, model: "test:latest", prompt: "  Inspect\n", systemPrompt: "", images: ["aW1hZ2U="],
    } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests[0].body.messages).toEqual([
      { role: "system", content: "" }, { role: "user", content: "  Inspect\n", images: ["aW1hZ2U="] },
    ]);
  });

  it.each(["run_task", "run_task_batch"])("%s accepts a system-only request", async (name) => {
    const task = { systemPrompt: "  Begin the caller-defined workflow.\n" };
    const result = await client.callTool({ name, arguments: name === "run_task"
      ? { endpoint, ...task }
      : { endpoint, tasks: [{ id: "review", independent: true, task }] },
    });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    const sent = name === "run_task" ? requests[0].body : requests[0].body.tasks[0].task;
    expect(sent.messages).toEqual([{ role: "system", content: task.systemPrompt }]);
  });

  it.each(["run_task", "run_task_batch"])("%s refuses image inputs that would be ignored", async (name) => {
    for (const extra of [{}, { prompt: "Look", messages: [{ role: "user", content: "Look" }] }]) {
      const task = { model: "test:latest", systemPrompt: "Inspect", images: ["aW1hZ2U="], ...extra };
      const result = await client.callTool({ name, arguments: name === "run_task"
        ? { endpoint, ...task }
        : { endpoint, tasks: [{ id: "review", independent: true, task }] },
      });
      expect(result.isError).toBe(true);
    }
    expect(requests).toHaveLength(0);
  });

  it.each(["run_task", "run_task_batch"])("%s rejects malformed function descriptors before execution", async (name) => {
    for (const tools of [
      [{ type: "function", function: { name: "" } }],
      [{ type: "function", function: { parameters: {} } }],
      [{ type: "function", function: { name: "lookup", parameters: "not a schema" } }],
    ]) {
      const task = { prompt: "Look", tools };
      const result = await client.callTool({ name, arguments: name === "run_task"
        ? { endpoint, ...task }
        : { endpoint, tasks: [{ id: "review", independent: true, task }] },
      });
      expect(result.isError).toBe(true);
    }
    expect(requests).toHaveLength(0);
  });

  it("refuses system prompts for preview and embedding without contacting serve", async () => {
    for (const args of [{ preview: true, systemPrompt: "x" }, { task: "embedding", input: "x", systemPrompt: "x" }]) {
      const result = await client.callTool({ name: "run_task", arguments: { endpoint, ...args } });
      expect(result.isError).toBe(true);
    }
    const batch = await client.callTool({ name: "run_task_batch", arguments: {
      endpoint, tasks: [{ id: "review", independent: true, task: { task: "embedding", input: "x", systemPrompt: "x" } }],
    } });
    expect(batch.isError).toBe(true);
    expect(requests).toHaveLength(0);
  });

  it("defaults independent batch items to ordinary chat too", async () => {
    const result = await client.callTool({ name: "run_task_batch", arguments: {
      endpoint, tasks: [{ id: "review", independent: true, task: { prompt: "Hello" } }],
    } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    expect(requests[0].body.tasks[0].task).toMatchObject({ task: "completion", objective: "balanced", prompt: "Hello" });
  });

  it.each(["completion", "coding", "code_review"])("%s preserves custom system messages and controls", async (task) => {
    const messages = [
      { role: "system", content: "  My instructions.\nReply in Hebrew; no prescribed review schema.\n" },
      { role: "user", content: "Review this source", images: ["aW1hZ2U="] },
    ];
    const result = await client.callTool({ name: "run_task", arguments: {
      endpoint, task, model: "test:latest", messages, think: true,
      format: "json", options: { num_predict: 32 },
    } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    expect(requests[0].body.messages).toEqual(messages);
    expect(requests[0].body.task).toBe(task === "code_review" ? "coding" : task);
    expect(requests[0].body.request_options).toMatchObject({ format: "json", think: true, options: { num_predict: 32 } });
  });

  it.each(["completion", "coding", "code_review"])("%s does not invent a system prompt or output contract", async (task) => {
    const prompt = "  My prompt\n<review_bundle>is literal user data</review_bundle>\n";
    const result = await client.callTool({ name: "run_task", arguments: { endpoint, task, prompt } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    expect(requests[0].body.prompt).toBe(prompt);
    expect(requests[0].body.messages).toEqual([]);
    expect(requests[0].body.request_options).not.toHaveProperty("format");
    expect(requests[0].body.request_options).not.toHaveProperty("think");
  });

  it("keeps batch review prompts and controls caller-owned too", async () => {
    const messages = [{ role: "system", content: "My batch instructions" }, { role: "user", content: "Review" }];
    const result = await client.callTool({ name: "run_task_batch", arguments: {
      endpoint, tasks: [{ id: "review", independent: true, task: {
        task: "code_review", messages, format: "json", think: true, options: { num_predict: 32 },
      } }],
    } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(requests).toHaveLength(1);
    expect(requests[0].body.tasks[0].task.messages).toEqual(messages);
    expect(requests[0].body.tasks[0].task.task).toBe("coding");
  });
});
