// The gateway's HTTP side. Virtual models (virtual.ts) are listed first in `/api/tags` and
// `/v1/models` and described by `/api/show`. A streamed request on one (Ollama's default), and
// every Anthropic Messages or OpenAI Responses request on one, is rewritten and sent to Ollama's
// own endpoint through serve. A non-streaming Ollama or OpenAI chat runs as a managed task
// (managed.ts). Every other request, real tags included, goes through to serve unchanged.
// Every response for a virtual model names the requested virtual model in `model` and ends with a
// `freellama` receipt (the real tag and the result checks) on the protocol's final event.
//
// Over HTTP a request never names files and the gateway fetches no URLs, so a page that can reach
// loopback gains nothing beyond what Ollama's own API gives it. Non-loopback Origin and Host
// headers are refused, as Ollama does, which also blocks DNS rebinding.
import { createHash } from "node:crypto";
import http from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import type { ReadableStream } from "node:stream/web";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { ensureServe } from "../serve.js";
import { answerText, fromOllamaRequest, fromOpenAiRequest, ollamaResponse, openAiCompletion, runManagedChat } from "./managed.js";
import { record, withoutUndefined, type Fields } from "./util.js";
import { GatewayError, prepareForward, streamChecks, virtualModel, virtualModels, type Resolution, type VirtualModel } from "./virtual.js";

type GatewayOptions = {
  host?: string;
  port?: number;
  serveEndpoint?: string;
  /** Append check: lines to the answer text, for a person reading it in a chat. */
  checksInContent?: boolean;
  log?: (line: string) => void;
};

const MAX_BODY_BYTES = 64 * 1024 * 1024;
const LOOPBACK = new Set(["127.0.0.1", "localhost", "[::1]", "::1"]);
const NATIVE = new Set(["/api/chat", "/api/generate"]);
/** The OpenAI and Anthropic layers, whose streams are server-sent events. */
const SSE_PATHS = new Set(["/v1/chat/completions", "/v1/messages", "/v1/responses"]);
const VIRTUAL_POSTS = new Set(["/api/chat", "/api/generate", "/api/show", "/v1/chat/completions", "/v1/messages", "/v1/responses"]);
/** Client headers an Ollama-compatible endpoint reads; the rest stay with the gateway. */
const FORWARDED_HEADERS = ["authorization", "x-api-key", "anthropic-version", "anthropic-beta"];
const HOP_HEADERS = new Set(["content-length", "content-encoding", "transfer-encoding", "connection"]);

/** A browser page on another origin is refused; non-browser clients send no Origin. */
export function originAllowed(origin: string | undefined): boolean {
  if (origin === undefined) return true;
  try {
    return LOOPBACK.has(new URL(origin).hostname);
  } catch {
    return false;
  }
}

/** A Host naming anything but loopback is a rebound DNS name pointing a page at this port. */
export function hostAllowed(host: string | undefined): boolean {
  if (host === undefined) return true;
  try {
    return LOOPBACK.has(new URL(`http://${host}`).hostname);
  } catch {
    return false;
  }
}

/** The request body, at most MAX_BODY_BYTES; a larger one is drained and refused with 413. */
function readBody(request: http.IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    request.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size <= MAX_BODY_BYTES) chunks.push(chunk);
    });
    request.on("end", () => (size > MAX_BODY_BYTES ? reject(new GatewayError(`request body over ${MAX_BODY_BYTES} bytes`, 413)) : resolve(Buffer.concat(chunks))));
    request.on("error", reject);
  });
}

function sendJson(response: http.ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

/** serve, started when needed; a start that fails is a 503 the client can retry. */
async function serveReady(endpoint: string | undefined): Promise<void> {
  try {
    await ensureServe(endpoint);
  } catch (error) {
    throw new GatewayError((error as Error).message, 503);
  }
}

/** Forward a request to serve as is, streaming both ways; the upstream request ends when the client leaves. */
async function proxy(request: http.IncomingMessage, response: http.ServerResponse, serveEndpoint: string, body?: Buffer): Promise<void> {
  const target = new URL(request.url ?? "/", serveEndpoint);
  const upstream = http.request(target, { method: request.method, headers: { ...request.headers, host: target.host } });
  response.on("close", () => { if (!response.writableFinished) upstream.destroy(); });
  const reply = await new Promise<http.IncomingMessage>((resolve, reject) => {
    upstream.once("response", resolve);
    upstream.once("error", reject);
    if (body !== undefined) upstream.end(body);
    else request.pipe(upstream);
  }).catch((error: Error) => {
    throw new GatewayError(`freellama serve at ${serveEndpoint} did not answer: ${error.message}`, 502);
  });
  response.writeHead(reply.statusCode ?? 502, reply.headers);
  await pipeline(reply, response);
}

async function serveJson(serveEndpoint: string, pathname: string): Promise<Fields | undefined> {
  try {
    const reply = await fetch(new URL(pathname, serveEndpoint), { signal: AbortSignal.timeout(5_000) });
    return reply.ok ? record(await reply.json()) : undefined;
  } catch {
    return undefined;
  }
}

const DETAILS = { parent_model: "", format: "freellama", family: "freellama", families: ["freellama"], parameter_size: "", quantization_level: "" };

function ollamaTag(model: VirtualModel, now: string): Fields {
  // The ollama CLI prints digest[:12]; a stable hash of the name keeps `ollama list` working.
  return { name: `${model.name}:latest`, model: `${model.name}:latest`, modified_at: now, size: 0, digest: createHash("sha256").update(model.name).digest("hex"), details: DETAILS };
}

function ollamaShow(model: VirtualModel): Fields {
  return {
    license: "",
    modelfile: `# FreeLlama virtual model: ${model.title}\n# Runs on the tested pick that is installed and fits now; see \`freellama workflows\`.`,
    parameters: "",
    template: "{{ .Prompt }}",
    details: DETAILS,
    model_info: {},
    capabilities: ["completion", ...(model.vision ? ["vision"] : [])],
    modified_at: new Date().toISOString(),
  };
}

/** Ollama's preload request (no messages, or no prompt and no images): answered at once. */
function isLoadOnly(body: Fields, path: string): boolean {
  if (path === "/api/chat") return !Array.isArray(body.messages) || body.messages.length === 0;
  return (typeof body.prompt !== "string" || body.prompt === "") && !(Array.isArray(body.images) && body.images.length > 0);
}

/** Whether a virtual-model request runs on Ollama's own endpoint (streamed or a protocol only Ollama encodes). */
function forwarded(path: string, body: Fields): boolean {
  if (NATIVE.has(path)) return body.stream !== false;
  return body.stream === true || path === "/v1/messages" || path === "/v1/responses";
}

export async function startGateway(options: GatewayOptions = {}): Promise<{ url: string; port: number; close: () => Promise<void> }> {
  const serveEndpoint = options.serveEndpoint ?? DEFAULT_SERVE_ENDPOINT;
  const explicitEndpoint = options.serveEndpoint;
  const log = options.log ?? (() => undefined);
  const checksInContent = options.checksInContent === true;

  /** `/api/tags` and `/v1/models`: virtual models first, then what serve lists. */
  async function listModels(path: string, response: http.ServerResponse): Promise<void> {
    await serveReady(explicitEndpoint);
    const real = await serveJson(serveEndpoint, path);
    if (path === "/api/tags") {
      const now = new Date().toISOString();
      return sendJson(response, 200, { models: [...virtualModels().map((model) => ollamaTag(model, now)), ...(Array.isArray(real?.models) ? real.models : [])] });
    }
    const created = Math.floor(Date.now() / 1000);
    sendJson(response, 200, { object: "list", data: [...virtualModels().map((model) => ({ id: model.name, object: "model", created, owned_by: "freellama" })), ...(Array.isArray(real?.data) ? real.data : [])] });
  }

  /** A non-streaming Ollama or OpenAI chat as a managed task. */
  async function managed(path: string, model: VirtualModel, body: Fields, response: http.ServerResponse, signal: AbortSignal): Promise<void> {
    const requested = String(body.model);
    const chat = path === "/v1/chat/completions" ? fromOpenAiRequest(body) : fromOllamaRequest(body, path);
    const answer = await runManagedChat(model, chat, { serveEndpoint: explicitEndpoint, signal });
    log(`${requested} -> ${answer.model} (${path}, managed)${answer.checks.length ? `; ${answer.checks.length} check(s)` : ""}`);
    const content = answerText(answer, checksInContent);
    sendJson(response, 200, path === "/v1/chat/completions" ? openAiCompletion(answer, requested, content) : ollamaResponse(answer, requested, path, content));
  }

  /** Rewrite a virtual-model request and send it to Ollama's own endpoint through serve. */
  async function forward(path: string, model: VirtualModel, body: Fields, request: http.IncomingMessage, response: http.ServerResponse, signal: AbortSignal): Promise<void> {
    const { body: rewritten, resolution, runner, numCtx } = await prepareForward(path, model, body, explicitEndpoint, signal);
    log(`${String(body.model)} -> ${resolution.model} (${path}, ${runner === "warmed" ? "loaded through admission, " : runner === "assessed" ? "memory assessed, " : ""}streamed by Ollama${numCtx ? `, num_ctx ${numCtx} from ${resolution.contextSource}` : ""})`);
    const headers: Record<string, string> = { "content-type": "application/json" };
    for (const name of FORWARDED_HEADERS) {
      const value = request.headers[name];
      if (typeof value === "string") headers[name] = value;
    }
    const reply = await fetch(new URL(path, serveEndpoint), { method: "POST", headers, body: JSON.stringify(rewritten), signal });
    if (!reply.body) {
      response.writeHead(reply.status);
      return void response.end();
    }
    // The caller sees the virtual name it asked for; the real tag goes in the `freellama` receipt.
    const requested = String(body.model);
    const tools = Array.isArray(body.tools) ? body.tools : undefined;
    const type = reply.headers.get("content-type") ?? "";
    const replyHeaders = Object.fromEntries([...reply.headers].filter(([name]) => !HOP_HEADERS.has(name)));
    if (reply.ok && NATIVE.has(path) && type.includes("ndjson")) {
      response.writeHead(200, { "content-type": "application/x-ndjson" });
      return pipeline(Readable.fromWeb(reply.body as ReadableStream<Uint8Array>), withChecksOnDone(path, requested, resolution, tools, checksInContent), response);
    }
    if (reply.ok && SSE_PATHS.has(path) && type.includes("event-stream")) {
      response.writeHead(200, replyHeaders);
      return pipeline(Readable.fromWeb(reply.body as ReadableStream<Uint8Array>), sseWithReceipt(path, requested, resolution, tools), response);
    }
    if (reply.ok && SSE_PATHS.has(path) && type.includes("json")) {
      const text = await reply.text();
      let payload: Fields | undefined;
      try {
        payload = record(JSON.parse(text));
      } catch {
        payload = undefined;
      }
      response.writeHead(200, replyHeaders);
      return void response.end(payload ? JSON.stringify(jsonWithReceipt(path, requested, resolution, tools, payload)) : text);
    }
    response.writeHead(reply.status, replyHeaders);
    return pipeline(Readable.fromWeb(reply.body as ReadableStream<Uint8Array>), response);
  }

  const server = http.createServer(async (request, response) => {
    const abort = new AbortController();
    response.on("close", () => { if (!response.writableFinished) abort.abort(); });
    try {
      if (!originAllowed(request.headers.origin) || !hostAllowed(request.headers.host)) return sendJson(response, 403, { error: "origin not allowed" });
      const path = new URL(request.url ?? "/", "http://gateway").pathname;
      // The ollama CLI's heartbeat (HEAD /) needs only a 200.
      if ((request.method === "GET" || request.method === "HEAD") && path === "/") {
        response.writeHead(200, { "content-type": "text/plain" });
        return void response.end(request.method === "HEAD" ? undefined : "FreeLlama gateway is running");
      }
      if (request.method === "GET" && (path === "/api/tags" || path === "/v1/models")) return await listModels(path, response);
      if (request.method !== "POST" || !VIRTUAL_POSTS.has(path)) {
        await serveReady(explicitEndpoint);
        return await proxy(request, response, serveEndpoint);
      }
      const raw = await readBody(request);
      let body: Fields;
      try {
        body = record(JSON.parse(raw.toString("utf8") || "{}")) ?? {};
      } catch {
        return sendJson(response, 400, { error: "request body is not JSON" });
      }
      const model = virtualModel(body.model ?? body.name);
      if (!model) {
        await serveReady(explicitEndpoint);
        return await proxy(request, response, serveEndpoint, raw);
      }
      if (path === "/api/show") return sendJson(response, 200, ollamaShow(model));
      if (NATIVE.has(path) && isLoadOnly(body, path)) {
        const done = { model: String(body.model), created_at: new Date().toISOString(), done: true, done_reason: "load" };
        return sendJson(response, 200, path === "/api/chat" ? { ...done, message: { role: "assistant", content: "" } } : { ...done, response: "" });
      }
      if (!forwarded(path, body)) return await managed(path, model, body, response, abort.signal);
      await serveReady(explicitEndpoint);
      await forward(path, model, body, request, response, abort.signal);
    } catch (error) {
      // A client that left mid-stream needs no answer.
      if (abort.signal.aborted) return void response.destroy();
      const status = error instanceof GatewayError ? error.status : 500;
      log(`error ${status}: ${(error as Error).message.slice(0, 300)}`);
      if (!response.headersSent) sendJson(response, status, { error: (error as Error).message });
      else response.destroy();
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port ?? 11437, options.host ?? "127.0.0.1", () => resolve());
  });
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : options.port ?? 11437;
  return {
    url: `http://${options.host ?? "127.0.0.1"}:${port}`,
    port,
    close: () => new Promise<void>((resolve) => {
      server.close(() => resolve());
      server.closeAllConnections();
    }),
  };
}

/** A tool call in Ollama's shape, the one result checks read. */
type ToolCall = { function: { name: string; arguments: unknown } };

/**
 * The `freellama` receipt a virtual-model response ends with: the real tag, the result checks for
 * the whole answer and its tool calls, and the job. Only the native API sends a context window, so
 * only its receipt names one.
 */
function receiptOf(resolution: Resolution, tools: unknown[] | undefined, native: boolean) {
  const schemas = toolSchemas(tools);
  return (answer: string, toolCalls: ToolCall[]): Fields => withoutUndefined({
    model: resolution.model,
    checks: streamChecks(resolution, answer, schemas, toolCalls),
    job: resolution.job,
    ...(native ? { num_ctx: resolution.numCtx, context_source: resolution.contextSource } : {}),
  });
}

/** Declared tools in the shape result checks read (`{function: {name, parameters}}`): OpenAI Chat's as is, Anthropic's and Responses' mapped. */
function toolSchemas(tools: unknown[] | undefined): unknown[] | undefined {
  return tools?.map((tool) => {
    const fields = record(tool);
    if (!fields || record(fields.function) || typeof fields.name !== "string") return tool;
    return { type: "function", function: { name: fields.name, parameters: record(fields.input_schema) ?? record(fields.parameters) ?? {} } };
  });
}

/**
 * Ollama's NDJSON, passed through line by line (a line split across chunks is joined first). Each
 * line names the requested virtual model. The done line gains a `freellama` receipt; for a person in
 * a chat the checks also arrive as one last content chunk.
 */
export function withChecksOnDone(path: string, requested: string, resolution: Resolution, tools: unknown[] | undefined, checksInContent: boolean) {
  const receipt = receiptOf(resolution, tools, true);
  return async function* (source: AsyncIterable<Buffer | Uint8Array>): AsyncGenerator<string> {
    const decoder = new TextDecoder();
    let pending = "";
    let answer = "";
    const calls: ToolCall[] = [];
    function* lineOut(line: string): Generator<string> {
      if (line.trim() === "") return;
      let chunk: Fields;
      try {
        chunk = JSON.parse(line) as Fields;
      } catch {
        yield `${line}\n`;
        return;
      }
      const message = record(chunk.message);
      const piece = path === "/api/chat" ? message?.content : chunk.response;
      if (typeof piece === "string") answer += piece;
      for (const call of Array.isArray(message?.tool_calls) ? message.tool_calls : []) {
        const fn = record(record(call)?.function);
        if (fn) calls.push({ function: { name: String(fn.name ?? ""), arguments: fn.arguments } });
      }
      const renamed = typeof chunk.model === "string" && chunk.model !== requested;
      if (renamed) chunk = { ...chunk, model: requested };
      if (chunk.done !== true) {
        yield renamed ? `${JSON.stringify(chunk)}\n` : `${line}\n`;
        return;
      }
      const freellama = receipt(answer, calls);
      const checks = freellama.checks as string[];
      if (checksInContent && checks.length > 0) {
        const note = `\n\n${checks.map((check) => `check: ${check}`).join("\n")}`;
        yield `${JSON.stringify({ model: requested, created_at: chunk.created_at, ...(path === "/api/chat" ? { message: { role: "assistant", content: note } } : { response: note }), done: false })}\n`;
      }
      yield `${JSON.stringify({ ...chunk, freellama })}\n`;
    }
    for await (const bytes of source) {
      pending += decoder.decode(bytes, { stream: true });
      const lines = pending.split("\n");
      pending = lines.pop() ?? "";
      for (const line of lines) yield* lineOut(line);
    }
    yield* lineOut(pending + decoder.decode());
  };
}

/** One server-sent event: its `event:` name, its joined `data:` lines, and its lines as received. */
type SseEvent = { event?: string; data: string; lines: string[] };
type SseHandler = { event(event: SseEvent): string; end(): string };

const sseOut = (event: string | undefined, data: unknown): string => `${event ? `event: ${event}\n` : ""}data: ${JSON.stringify(data)}\n\n`;
const sseRaw = (event: SseEvent): string => `${event.lines.join("\n")}\n\n`;

/**
 * Server-sent events parsed across chunk boundaries (an event or a line split mid-way is joined
 * first; CRLF is accepted). Each complete event goes to the handler, and what it returns is written.
 */
function sseTransform(handler: SseHandler) {
  return async function* (source: AsyncIterable<Buffer | Uint8Array>): AsyncGenerator<string> {
    const decoder = new TextDecoder();
    let pending = "";
    let lines: string[] = [];
    function flush(): string {
      if (lines.length === 0) return "";
      const event: SseEvent = { data: "", lines };
      const data: string[] = [];
      for (const line of lines) {
        if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
        else if (line.startsWith("event:")) event.event = line.slice(6).trim();
      }
      event.data = data.join("\n");
      lines = [];
      return handler.event(event);
    }
    function lineIn(raw: string): string {
      const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
      if (line !== "") {
        lines.push(line);
        return "";
      }
      return flush();
    }
    for await (const bytes of source) {
      pending += decoder.decode(bytes, { stream: true });
      const parts = pending.split("\n");
      pending = parts.pop() ?? "";
      const out = parts.map(lineIn).join("");
      if (out) yield out;
    }
    const rest = pending + decoder.decode();
    const out = (rest ? lineIn(rest) : "") + flush() + handler.end();
    if (out) yield out;
  };
}

const parseObject = (data: string): Fields | undefined => {
  try {
    return record(JSON.parse(data));
  } catch {
    return undefined;
  }
};

/** Tool calls gathered from fragments: a name, and arguments as a JSON string built up or an object. */
class CallParts {
  private readonly parts = new Map<number | string, { name: string; json: string; input?: unknown }>();
  add(key: number | string, name: unknown, input?: unknown): void {
    const part = this.parts.get(key) ?? { name: "", json: "" };
    if (typeof name === "string" && name !== "") part.name = name;
    if (input !== undefined) part.input = input;
    this.parts.set(key, part);
  }
  append(key: number | string, json: unknown): void {
    if (typeof json !== "string") return;
    const part = this.parts.get(key) ?? { name: "", json: "" };
    part.json += json;
    this.parts.set(key, part);
  }
  calls(): ToolCall[] {
    return [...this.parts.values()].map((part) => ({ function: { name: part.name, arguments: part.json !== "" ? part.json : part.input ?? {} } }));
  }
}

/**
 * OpenAI Chat chunks: each names the requested virtual model. The receipt rides on the last chunk
 * before `data: [DONE]`. A chunk that may be last (a finish reason, usage or no choices) is held
 * until the next event shows whether it was; the token chunks before it stream without delay.
 */
function openAiChatHandler(requested: string, receipt: ReturnType<typeof receiptOf>): SseHandler {
  let answer = "";
  const calls = new CallParts();
  let held: { event?: string; chunk: Fields } | undefined;
  let last: Fields | undefined;
  let finished = false;
  const release = (): string => {
    const out = held ? sseOut(held.event, held.chunk) : "";
    held = undefined;
    return out;
  };
  const finish = (): string => {
    if (finished) return "";
    finished = true;
    const freellama = receipt(answer, calls.calls());
    if (held) {
      const out = sseOut(held.event, { ...held.chunk, freellama });
      held = undefined;
      return out;
    }
    // A stream without a final chunk: a chunk with no choices, the shape OpenAI's usage chunk has.
    return last ? sseOut(undefined, { id: last.id, object: last.object ?? "chat.completion.chunk", created: last.created, model: requested, choices: [], freellama }) : "";
  };
  return {
    event(event) {
      if (event.data.trim() === "[DONE]") return finish() + sseRaw(event);
      const chunk = parseObject(event.data);
      if (!chunk) return release() + sseRaw(event);
      if (typeof chunk.model === "string") chunk.model = requested;
      const choices = Array.isArray(chunk.choices) ? chunk.choices.map(record) : undefined;
      for (const choice of choices ?? []) {
        const delta = record(choice?.delta);
        if (typeof delta?.content === "string") answer += delta.content;
        for (const call of Array.isArray(delta?.tool_calls) ? delta.tool_calls.map(record) : []) {
          const key = typeof call?.index === "number" ? call.index : String(call?.id ?? "");
          const fn = record(call?.function);
          calls.add(key, fn?.name);
          if (typeof fn?.arguments === "string") calls.append(key, fn.arguments);
          else if (fn?.arguments !== undefined) calls.add(key, undefined, fn.arguments);
        }
      }
      last = chunk;
      const mayBeLast = chunk.usage !== undefined || (choices !== undefined && (choices.length === 0 || choices.some((choice) => choice?.finish_reason != null)));
      const out = release();
      if (mayBeLast && !finished) {
        held = { event: event.event, chunk };
        return out;
      }
      return out + sseOut(event.event, chunk);
    },
    end: () => finish(),
  };
}

/**
 * OpenAI Responses events: the response object names the requested virtual model, and the receipt
 * rides on `response.completed` (or `response.incomplete`), beside `response`.
 */
function responsesHandler(requested: string, receipt: ReturnType<typeof receiptOf>): SseHandler {
  let answer = "";
  const calls: ToolCall[] = [];
  return {
    event(event) {
      const payload = parseObject(event.data);
      if (!payload) return sseRaw(event);
      const response = record(payload.response);
      if (response && typeof response.model === "string") payload.response = { ...response, model: requested };
      const type = payload.type ?? event.event;
      if (type === "response.output_text.delta" && typeof payload.delta === "string") answer += payload.delta;
      const item = record(payload.item);
      if (type === "response.output_item.done" && item?.type === "function_call") calls.push({ function: { name: String(item.name ?? ""), arguments: item.arguments ?? {} } });
      if (type === "response.completed" || type === "response.incomplete") payload.freellama = receipt(answer, calls);
      return sseOut(event.event, payload);
    },
    end: () => "",
  };
}

/**
 * Anthropic Messages events: `message_start` names the requested virtual model, and the receipt
 * rides on `message_stop`. Text deltas are the answer; tool_use blocks and their JSON deltas are
 * its tool calls.
 */
function anthropicHandler(requested: string, receipt: ReturnType<typeof receiptOf>): SseHandler {
  let answer = "";
  const calls = new CallParts();
  return {
    event(event) {
      const payload = parseObject(event.data);
      if (!payload) return sseRaw(event);
      const type = payload.type ?? event.event;
      const message = record(payload.message);
      if (message && typeof message.model === "string") payload.message = { ...message, model: requested };
      const index = typeof payload.index === "number" ? payload.index : -1;
      const block = record(payload.content_block);
      if (type === "content_block_start" && block?.type === "tool_use") calls.add(index, block.name, record(block.input) && Object.keys(block.input as Fields).length > 0 ? block.input : undefined);
      if (type === "content_block_start" && block?.type === "text" && typeof block.text === "string") answer += block.text;
      const delta = record(payload.delta);
      if (type === "content_block_delta" && delta?.type === "text_delta" && typeof delta.text === "string") answer += delta.text;
      if (type === "content_block_delta" && delta?.type === "input_json_delta") calls.append(index, delta.partial_json);
      if (type === "message_stop") payload.freellama = receipt(answer, calls.calls());
      return sseOut(event.event, payload);
    },
    end: () => "",
  };
}

/**
 * A virtual model's SSE stream on the OpenAI or Anthropic layer, with the requested virtual name in
 * `model` and a `freellama` receipt on the protocol's final event. Content passes unchanged: check
 * lines go only in the receipt, so a tool-using client's content stays clean.
 */
export function sseWithReceipt(path: string, requested: string, resolution: Resolution, tools: unknown[] | undefined) {
  const receipt = receiptOf(resolution, tools, false);
  const handler = path === "/v1/messages" ? anthropicHandler(requested, receipt)
    : path === "/v1/responses" ? responsesHandler(requested, receipt)
    : openAiChatHandler(requested, receipt);
  return sseTransform(handler);
}

/** A virtual model's non-streamed OpenAI or Anthropic response: the requested virtual name in `model`, and a `freellama` receipt. */
export function jsonWithReceipt(path: string, requested: string, resolution: Resolution, tools: unknown[] | undefined, payload: Fields): Fields {
  let answer = "";
  const calls: ToolCall[] = [];
  if (path === "/v1/messages") {
    for (const block of (Array.isArray(payload.content) ? payload.content : []).map(record)) {
      if (block?.type === "text" && typeof block.text === "string") answer += block.text;
      if (block?.type === "tool_use") calls.push({ function: { name: String(block.name ?? ""), arguments: block.input ?? {} } });
    }
  } else if (path === "/v1/responses") {
    for (const item of (Array.isArray(payload.output) ? payload.output : []).map(record)) {
      if (item?.type === "function_call") calls.push({ function: { name: String(item.name ?? ""), arguments: item.arguments ?? {} } });
      for (const part of (Array.isArray(item?.content) ? item.content : []).map(record)) {
        if (part?.type === "output_text" && typeof part.text === "string") answer += part.text;
      }
    }
  } else {
    const message = record(record((Array.isArray(payload.choices) ? payload.choices : [])[0])?.message);
    if (typeof message?.content === "string") answer = message.content;
    for (const call of (Array.isArray(message?.tool_calls) ? message.tool_calls : []).map(record)) {
      const fn = record(call?.function);
      if (fn) calls.push({ function: { name: String(fn.name ?? ""), arguments: fn.arguments ?? {} } });
    }
  }
  return { ...payload, ...(typeof payload.model === "string" ? { model: requested } : {}), freellama: receiptOf(resolution, tools, false)(answer, calls) };
}
