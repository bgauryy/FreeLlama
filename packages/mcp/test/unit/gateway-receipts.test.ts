import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// Gateway receipts on every protocol: the requested virtual name in `model`, the real tag and the
// result checks in `freellama`. The transforms are fed recorded Ollama chunks; the HTTP tests run the
// real gateway against a fake serve, with only serve and the live inventory faked.
const fixture = vi.hoisted(() => ({ live: vi.fn(), route: vi.fn(), warm: vi.fn() }));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: vi.fn(), getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: (...args: unknown[]) => fixture.route(...args),
  createSession: vi.fn(), deleteSession: vi.fn(), killSession: vi.fn(), listModels: vi.fn(), warmModelRequest: (...args: unknown[]) => fixture.warm(...args),
  listTaskJobs: vi.fn(), createScope: vi.fn(), getScope: vi.fn(), forkScope: vi.fn(), deleteScope: vi.fn(),
}));
vi.mock("../../src/model-library.js", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  catalogLiveState: (...args: unknown[]) => fixture.live(...args),
}));

import { jsonWithReceipt, sseWithReceipt, startGateway, withChecksOnDone } from "../../src/agent-cli/gateway.js";
import type { Resolution } from "../../src/agent-cli/virtual.js";

let savedAutostart: string | undefined;
beforeAll(() => {
  savedAutostart = process.env.FREELLAMA_MCP_AUTOSTART_SERVE;
  process.env.FREELLAMA_MCP_AUTOSTART_SERVE = "0";
});
afterAll(() => {
  if (savedAutostart === undefined) delete process.env.FREELLAMA_MCP_AUTOSTART_SERVE;
  else process.env.FREELLAMA_MCP_AUTOSTART_SERVE = savedAutostart;
});

const REAL = "bonsai-2:9b";
const VIRTUAL = "freellama/summarize-text";
const resolution: Resolution = { model: REAL, options: {}, task: "completion", job: "summarize", numCtx: 8192, contextSource: "route" };

/** Run a transform over chunks as they would arrive from Ollama, and return what the client reads. */
async function run(transform: (source: AsyncIterable<Buffer>) => AsyncGenerator<string>, chunks: string[]): Promise<string> {
  async function* source() { for (const chunk of chunks) yield Buffer.from(chunk); }
  let out = "";
  for await (const piece of transform(source())) out += piece;
  return out;
}

/** The data payloads of an SSE stream, `[DONE]` as a string. */
function events(text: string): Array<{ event?: string; data: any }> {
  return text.split("\n\n").filter((block) => block.trim() !== "").map((block) => {
    const lines = block.split("\n");
    const event = lines.find((line) => line.startsWith("event:"))?.slice(6).trim();
    const data = lines.filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trim()).join("\n");
    return { event, data: data === "[DONE]" ? data : JSON.parse(data) };
  });
}

/** Split a stream at fixed points, so events and lines arrive cut mid-way. */
const cut = (text: string, at: number[]): string[] => [0, ...at].map((start, index) => text.slice(start, [...at, text.length][index]));

const chunk = (delta: Record<string, unknown>, finish: string | null = null) =>
  `data: ${JSON.stringify({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: REAL, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;

describe("OpenAI chat completion streams", () => {
  const recorded = chunk({ role: "assistant", content: "Two lines. " }) + chunk({ content: "Let me know if you need more." }) + chunk({}, "stop") + "data: [DONE]\n\n";

  it("names the virtual model on every chunk and puts the receipt on the last chunk before [DONE], across split events", async () => {
    const text = await run(sseWithReceipt("/v1/chat/completions", VIRTUAL, resolution, undefined), cut(recorded, [17, 140, 141, 250]));
    const out = events(text);
    expect(out.at(-1)!.data).toBe("[DONE]");
    const data = out.slice(0, -1).map((item) => item.data);
    expect(data.map((item) => item.model)).toEqual([VIRTUAL, VIRTUAL, VIRTUAL]);
    expect(data.slice(0, -1).every((item) => item.freellama === undefined)).toBe(true);
    expect(data.at(-1)).toMatchObject({ choices: [{ finish_reason: "stop" }], freellama: { model: REAL, job: "summarize" } });
    expect(data.at(-1).freellama.checks.join(" ")).toMatch(/filler/);
    // The OpenAI layer runs at Ollama's default window, so no window is reported.
    expect(data.at(-1).freellama.num_ctx).toBeUndefined();
    // Content passes unchanged: no check text is injected.
    expect(data.map((item) => item.choices[0].delta.content ?? "").join("")).toBe("Two lines. Let me know if you need more.");
  });

  it("accepts CRLF line endings", async () => {
    const text = await run(sseWithReceipt("/v1/chat/completions", VIRTUAL, resolution, undefined), [recorded.replaceAll("\n", "\r\n")]);
    expect(events(text).at(-2)!.data.freellama.model).toBe(REAL);
  });

  it("holds the finish chunk until a usage chunk shows it was not last", async () => {
    const usage = `data: ${JSON.stringify({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: REAL, choices: [], usage: { total_tokens: 3 } })}\n\n`;
    const out = events(await run(sseWithReceipt("/v1/chat/completions", VIRTUAL, resolution, undefined), [chunk({ content: "ok" }) + chunk({}, "stop"), usage, "data: [DONE]\n\n"]));
    expect(out.map((item) => item.data === "[DONE]" ? "done" : item.data.freellama ? "receipt" : "chunk")).toEqual(["chunk", "chunk", "receipt", "done"]);
    expect(out[2]!.data.usage).toEqual({ total_tokens: 3 });
  });

  it("checks streamed tool calls, arguments assembled from fragments, against the declared tools", async () => {
    const tools = [{ type: "function", function: { name: "read_file", parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] } } }];
    const call = (args: string, name?: string) => chunk({ tool_calls: [{ index: 0, ...(name ? { id: "c1", type: "function" } : {}), function: { ...(name ? { name } : {}), arguments: args } }] });
    const text = await run(sseWithReceipt("/v1/chat/completions", VIRTUAL, resolution, tools), [call('{"pa', "read_file"), call('th_x":1}'), chunk({}, "tool_calls"), "data: [DONE]\n\n"]);
    expect(events(text).at(-2)!.data.freellama.checks.join(" ")).toMatch(/call 1 \(read_file\) lacks required path/);
  });

  it("adds a receipt chunk with no choices when the stream ends without a final chunk", async () => {
    const out = events(await run(sseWithReceipt("/v1/chat/completions", VIRTUAL, resolution, undefined), [chunk({ content: "ok" })]));
    expect(out).toHaveLength(2);
    expect(out[1]!.data).toMatchObject({ model: VIRTUAL, choices: [], freellama: { model: REAL } });
  });
});

describe("OpenAI Responses streams", () => {
  const response = (status: string) => ({ id: "resp_1", object: "response", model: REAL, status, output: [] });
  const recorded = [
    `event: response.created\ndata: ${JSON.stringify({ type: "response.created", sequence_number: 0, response: response("in_progress") })}\n\n`,
    `event: response.output_text.delta\ndata: ${JSON.stringify({ type: "response.output_text.delta", sequence_number: 1, item_id: "msg_1", output_index: 0, content_index: 0, delta: "Short " })}\n\n`,
    `event: response.output_text.delta\ndata: ${JSON.stringify({ type: "response.output_text.delta", sequence_number: 2, item_id: "msg_1", output_index: 0, content_index: 0, delta: "summary." })}\n\n`,
    `event: response.completed\ndata: ${JSON.stringify({ type: "response.completed", sequence_number: 3, response: response("completed") })}\n\n`,
  ].join("");

  it("names the virtual model on the response object and puts the receipt on response.completed, across split events", async () => {
    const out = events(await run(sseWithReceipt("/v1/responses", VIRTUAL, resolution, undefined), cut(recorded, [9, 40, 200, 201, 400])));
    expect(out.map((item) => item.event)).toEqual(["response.created", "response.output_text.delta", "response.output_text.delta", "response.completed"]);
    expect(out[0]!.data.response.model).toBe(VIRTUAL);
    expect(out[3]!.data).toMatchObject({ type: "response.completed", response: { model: VIRTUAL }, freellama: { model: REAL, job: "summarize", checks: [] } });
    expect(out.slice(0, 3).every((item) => item.data.freellama === undefined)).toBe(true);
  });
});

describe("Anthropic Messages streams", () => {
  const event = (name: string, data: Record<string, unknown>) => `event: ${name}\ndata: ${JSON.stringify({ type: name, ...data })}\n\n`;
  const start = event("message_start", { message: { id: "msg_1", type: "message", role: "assistant", model: REAL, content: [], stop_reason: null, usage: { input_tokens: 5, output_tokens: 0 } } });

  it("names the virtual model in message_start and puts the receipt on message_stop, across split events", async () => {
    const recorded = start
      + event("content_block_start", { index: 0, content_block: { type: "text", text: "" } })
      + event("content_block_delta", { index: 0, delta: { type: "text_delta", text: "I hope this helps." } })
      + event("content_block_stop", { index: 0 })
      + event("message_delta", { delta: { stop_reason: "end_turn" }, usage: { output_tokens: 4 } })
      + event("message_stop", {});
    const out = events(await run(sseWithReceipt("/v1/messages", VIRTUAL, resolution, undefined), cut(recorded, [5, 60, 300, 301, 302, recorded.length - 10])));
    expect(out.map((item) => item.event)).toEqual(["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]);
    expect(out[0]!.data.message.model).toBe(VIRTUAL);
    expect(out[2]!.data.delta.text).toBe("I hope this helps.");
    expect(out[5]!.data).toMatchObject({ type: "message_stop", freellama: { model: REAL, job: "summarize" } });
    expect(out[5]!.data.freellama.checks.join(" ")).toMatch(/filler/);
    expect(out.slice(0, 5).every((item) => item.data.freellama === undefined)).toBe(true);
  });

  it("checks a tool_use block, its input built from JSON deltas, against Anthropic-shaped tools", async () => {
    const tools = [{ name: "read_file", input_schema: { type: "object", properties: { path: { type: "string" } }, required: ["path"] } }];
    const recorded = start
      + event("content_block_start", { index: 0, content_block: { type: "tool_use", id: "t1", name: "read_file", input: {} } })
      + event("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: '{"path":' } })
      + event("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: '"a.ts"}' } })
      + event("content_block_stop", { index: 0 })
      + event("message_stop", {});
    const ok = events(await run(sseWithReceipt("/v1/messages", VIRTUAL, resolution, tools), [recorded]));
    expect(ok.at(-1)!.data.freellama.checks.join(" ")).not.toMatch(/not one of the tools|lacks required/);
    const wrong = events(await run(sseWithReceipt("/v1/messages", VIRTUAL, resolution, tools), [recorded.replace('"read_file"', '"write_file"')]));
    expect(wrong.at(-1)!.data.freellama.checks.join(" ")).toMatch(/write_file\) is not one of the tools sent \(read_file\)/);
  });
});

describe("Ollama NDJSON streams", () => {
  it("names the virtual model on every line and keeps the receipt with the window on the done line", async () => {
    const line = (fields: Record<string, unknown>) => `${JSON.stringify({ model: REAL, created_at: "t", ...fields })}\n`;
    const recorded = line({ message: { role: "assistant", content: "Done." }, done: false }) + line({ message: { role: "assistant", content: "" }, done: true, done_reason: "stop" });
    const lines = (await run(withChecksOnDone("/api/chat", `${VIRTUAL}:latest`, resolution, undefined, false), cut(recorded, [30]))).trim().split("\n").map((item) => JSON.parse(item));
    expect(lines.map((item) => item.model)).toEqual([`${VIRTUAL}:latest`, `${VIRTUAL}:latest`]);
    expect(lines.at(-1).freellama).toMatchObject({ model: REAL, job: "summarize", num_ctx: 8192, context_source: "route" });
  });
});

describe("non-streamed OpenAI and Anthropic responses", () => {
  it("name the virtual model and carry the receipt", () => {
    const anthropic = jsonWithReceipt("/v1/messages", VIRTUAL, resolution, undefined, { id: "m", type: "message", model: REAL, content: [{ type: "text", text: "Fine." }] });
    expect(anthropic).toMatchObject({ model: VIRTUAL, content: [{ text: "Fine." }], freellama: { model: REAL, checks: [] } });
    const responses = jsonWithReceipt("/v1/responses", VIRTUAL, resolution, undefined, { id: "r", object: "response", model: REAL, output: [{ type: "message", content: [{ type: "output_text", text: "Feel free to ask." }] }] });
    expect(responses.model).toBe(VIRTUAL);
    expect((responses.freellama as { checks: string[] }).checks.join(" ")).toMatch(/filler/);
  });
});

describe("gateway receipts over HTTP", () => {
  let gateway: Awaited<ReturnType<typeof startGateway>>;
  let serve: import("node:http").Server;
  const sent: Array<{ path: string; body: Record<string, any> }> = [];
  beforeAll(async () => {
    const http = await import("node:http");
    serve = http.createServer((request, response) => {
      let raw = "";
      request.on("data", (part) => { raw += part; });
      request.on("end", () => {
        if (request.url === "/api/ps") return void response.writeHead(200, { "content-type": "application/json" }).end('{"models":[]}');
        const body = JSON.parse(raw || "{}");
        sent.push({ path: request.url ?? "", body });
        if (request.url === "/v1/messages" && body.stream !== true) {
          response.writeHead(200, { "content-type": "application/json" });
          return void response.end(JSON.stringify({ id: "m", type: "message", role: "assistant", model: REAL, content: [{ type: "text", text: "Fine." }], stop_reason: "end_turn" }));
        }
        if (request.url === "/v1/chat/completions") {
          response.writeHead(200, { "content-type": "text/event-stream" });
          const text = chunk({ content: "Fine." }) + chunk({}, "stop") + "data: [DONE]\n\n";
          response.write(text.slice(0, 50));
          setTimeout(() => response.end(text.slice(50)), 10);
          return;
        }
        response.writeHead(404, { "content-type": "application/json" }).end('{"error":"not found"}');
      });
    });
    await new Promise<void>((resolve) => serve.listen(0, "127.0.0.1", () => resolve()));
    gateway = await startGateway({ port: 0, serveEndpoint: `http://127.0.0.1:${(serve.address() as { port: number }).port}` });
  });
  afterAll(async () => {
    await gateway.close();
    await new Promise<void>((resolve) => serve.close(() => resolve()));
  });
  beforeEach(() => {
    sent.length = 0;
    fixture.live.mockReset();
    fixture.live.mockResolvedValue([[{ name: REAL, capabilities: ["completion"] }], 64e9]);
    fixture.route.mockReset();
    fixture.route.mockResolvedValue(JSON.stringify({ selected_model: REAL, options: { num_ctx: 4096 } }));
  });
  const post = (path: string, body: unknown) => fetch(`${gateway.url}${path}`, { method: "POST", body: JSON.stringify(body) });

  it("streams OpenAI chat completions with the virtual name and a receipt", async () => {
    const reply = await post("/v1/chat/completions", { model: "freellama/auto", stream: true, messages: [{ role: "user", content: "x" }] });
    expect(reply.headers.get("content-type")).toContain("text/event-stream");
    const out = events(await reply.text());
    expect(sent[0]!.body.model).toBe(REAL);
    expect(out.slice(0, -1).map((item) => item.data.model)).toEqual(["freellama/auto", "freellama/auto"]);
    expect(out.at(-2)!.data.freellama).toMatchObject({ model: REAL, checks: [] });
  });

  it("answers a non-streamed Anthropic message with the virtual name and a receipt", async () => {
    const reply = await post("/v1/messages", { model: "freellama/auto", max_tokens: 50, messages: [{ role: "user", content: "x" }] });
    expect(await reply.json()).toMatchObject({ model: "freellama/auto", content: [{ text: "Fine." }], freellama: { model: REAL } });
  });

  it("leaves a real tag's response untouched", async () => {
    const reply = await post("/v1/messages", { model: REAL, max_tokens: 50, messages: [{ role: "user", content: "x" }] });
    const body = await reply.json();
    expect(body.model).toBe(REAL);
    expect(body.freellama).toBeUndefined();
  });
});
