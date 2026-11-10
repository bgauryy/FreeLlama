// The gateway's HTTP side. Virtual models (virtual.ts) are listed first in `/api/tags` and
// `/v1/models` and described by `/api/show`. A streamed request on one (Ollama's default), and
// every Anthropic Messages or OpenAI Responses request on one, is rewritten and sent to Ollama's
// own endpoint through serve. A non-streaming Ollama or OpenAI chat runs as a managed task
// (managed.ts). Every other request, real tags included, goes through to serve unchanged.
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
import { admitRunner, GatewayError, managedShiftOff, REWRITERS, streamChecks, virtualModel, virtualModels, type Resolution, type VirtualModel } from "./virtual.js";

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
    modelfile: `# FreeLlama virtual model: ${model.title}\n# Runs on the tested pick that is installed and fits now; see \`fllama workflows\`.`,
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
    const { body: rewritten, resolution } = await REWRITERS[path]!(model, body, explicitEndpoint);
    // Only the native API carries the runner profile; the OpenAI and Anthropic layers run at
    // Ollama's defaults. A native request matches serve's managed runners so it reuses them.
    const native = NATIVE.has(path);
    if (native && rewritten.shift === undefined && await managedShiftOff(explicitEndpoint)) rewritten.shift = false;
    const numCtx = native ? record(rewritten.options)?.num_ctx as number | undefined : undefined;
    const runner = await admitRunner(resolution, native ? { numCtx } : undefined, explicitEndpoint, signal);
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
    const source = Readable.fromWeb(reply.body as ReadableStream<Uint8Array>);
    if (!reply.ok || !NATIVE.has(path) || reply.headers.get("content-type")?.includes("ndjson") !== true) {
      response.writeHead(reply.status, Object.fromEntries([...reply.headers].filter(([name]) => !HOP_HEADERS.has(name))));
      return pipeline(source, response);
    }
    response.writeHead(200, { "content-type": "application/x-ndjson" });
    await pipeline(source, withChecksOnDone(path, resolution, body, checksInContent), response);
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

/**
 * Ollama's NDJSON, passed through line by line (a line split across chunks is joined first). The
 * done line gains a `freellama` field with the real model and the result checks; for a person in a
 * chat the checks also arrive as one last content chunk.
 */
function withChecksOnDone(path: string, resolution: Resolution, body: Fields, checksInContent: boolean) {
  const tools = Array.isArray(body.tools) ? body.tools : undefined;
  return async function* (source: AsyncIterable<Buffer>): AsyncGenerator<string> {
    const decoder = new TextDecoder();
    let pending = "";
    let answer = "";
    function* lineOut(line: string): Generator<string> {
      if (line.trim() === "") return;
      let chunk: Fields;
      try {
        chunk = JSON.parse(line) as Fields;
      } catch {
        yield `${line}\n`;
        return;
      }
      const piece = path === "/api/chat" ? record(chunk.message)?.content : chunk.response;
      if (typeof piece === "string") answer += piece;
      if (chunk.done !== true) {
        yield `${line}\n`;
        return;
      }
      const checks = streamChecks(resolution, answer, tools);
      if (checksInContent && checks.length > 0) {
        const note = `\n\n${checks.map((check) => `check: ${check}`).join("\n")}`;
        yield `${JSON.stringify({ model: chunk.model, created_at: chunk.created_at, ...(path === "/api/chat" ? { message: { role: "assistant", content: note } } : { response: note }), done: false })}\n`;
      }
      yield `${JSON.stringify({ ...chunk, freellama: withoutUndefined({ model: resolution.model, checks, job: resolution.job, num_ctx: resolution.numCtx, context_source: resolution.contextSource }) })}\n`;
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
