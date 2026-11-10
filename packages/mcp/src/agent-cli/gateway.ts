// FreeLlama virtual models over the Ollama and OpenAI protocols. `freellama/<workflow>` and
// `freellama/<job>` run through run_task (model pick, prompts, options, admission, checks); every
// other request, real tags included, streams through to serve unchanged. So the `ollama` CLI
// (OLLAMA_HOST), Open WebUI, Continue, LangChain or an OpenAI SDK (base URL .../v1) get the
// workflows without learning MCP.
//
// Over HTTP a request never names files: the gateway reads nothing from disk, so a page that can
// reach loopback gains nothing beyond what Ollama's own API gives it. Browser origins other than
// loopback are refused, as Ollama does by default.
import { createHash } from "node:crypto";
import http from "node:http";
import { applyJob, pickLiveJobModel } from "../job-mode.js";
import { LOCAL_JOBS, RUN_TASK_JOBS, WORKFLOWS, WORKFLOW_NAMES, type JobName, type WorkflowName } from "../local-prompts.js";
import { ensureServe } from "../serve.js";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { callTool, resultText, type ToolResult } from "./registry.js";
import { refusalOf, splitAnswer } from "./output.js";
import { UsageError } from "./args.js";

export const VIRTUAL_PREFIX = "freellama/";
const CHAT_TASKS = new Set(["coding", "completion", "vision"]);

export type VirtualModel = { name: string; title: string; job?: JobName; workflow?: WorkflowName; vision: boolean };

/** The chat-shaped workflows and jobs (decision and embedding jobs need fields a chat cannot carry), plus `auto`. */
export function virtualModels(): VirtualModel[] {
  const models: VirtualModel[] = [{ name: `${VIRTUAL_PREFIX}auto`, title: "FreeLlama routing: the best installed model for a general chat", vision: false }];
  for (const workflow of WORKFLOW_NAMES) {
    const flow = WORKFLOWS[workflow] as { job: JobName; title: string };
    const task = LOCAL_JOBS[flow.job].task;
    if (CHAT_TASKS.has(task)) models.push({ name: `${VIRTUAL_PREFIX}${workflow}`, title: flow.title, job: flow.job, workflow, vision: task === "vision" });
  }
  for (const job of RUN_TASK_JOBS) {
    const task = LOCAL_JOBS[job].task;
    if (CHAT_TASKS.has(task)) models.push({ name: `${VIRTUAL_PREFIX}${job}`, title: `job ${job}: ${LOCAL_JOBS[job].notes.split(". ")[0]}`, job, vision: task === "vision" });
  }
  return models;
}

/** `freellama/fix-bug`, `freellama/fix-bug:latest` (the ollama CLI adds the tag) -> its virtual model. */
export function virtualModel(name: unknown): VirtualModel | undefined {
  if (typeof name !== "string" || !name.toLowerCase().startsWith(VIRTUAL_PREFIX)) return undefined;
  const bare = name.toLowerCase().replace(/:latest$/, "");
  return virtualModels().find((model) => model.name === bare);
}

type Message = { role: string; content?: string; images?: string[]; tool_calls?: unknown[]; [key: string]: unknown };
export type ChatRequest = {
  messages: Message[];
  format?: unknown;
  options?: Record<string, unknown>;
  think?: boolean | string;
  keepAlive?: string;
  tools?: unknown[];
};
export type ChatAnswer = {
  model: string;
  content: string;
  thinking?: string;
  toolCalls?: unknown[];
  checks: string[];
  /** Ollama's done_reason: `stop`, or `length` when the output budget ran out. */
  doneReason: string;
  metrics: Record<string, unknown>;
  receipt: Record<string, unknown>;
};

/** A refusal or failure with the HTTP status a client should see. */
export class GatewayError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

const record = (value: unknown): Record<string, unknown> | undefined =>
  value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;

/**
 * The run_task arguments for one chat turn of a virtual model. A single user turn with no system
 * message is a plain workflow call (prompt), so job mode applies everything it would over MCP.
 * A longer conversation is the caller's: job mode keeps it whole, so the workflow's system prompt
 * is prepended here only when the caller sent none, and the pick is fixed for the turn.
 */
export async function chatTaskArgs(model: VirtualModel, request: ChatRequest, serveEndpoint?: string): Promise<Record<string, unknown>> {
  const options = { ...request.options };
  let contextTokens: number | undefined;
  if (typeof options.num_ctx === "number") {
    contextTokens = options.num_ctx;
    delete options.num_ctx;
  }
  const common: Record<string, unknown> = {
    endpoint: serveEndpoint,
    format: request.format,
    options: Object.keys(options).length > 0 ? options : undefined,
    think: request.think,
    keepAlive: request.keepAlive,
    tools: request.tools?.length ? request.tools : undefined,
    contextTokens,
  };
  const target = model.workflow ? { workflow: model.workflow } : model.job ? { job: model.job } : { task: "completion", objective: "balanced" };
  const messages = request.messages.filter((message) => message.role !== "system" || (message.content ?? "") !== "");
  const hasSystem = messages.some((message) => message.role === "system");
  const last = messages.at(-1);
  if (!last) throw new GatewayError("the conversation has no messages", 400);
  if (!hasSystem && messages.length === 1 && last.role === "user") {
    return clean({ ...common, ...target, prompt: last.content || "(see the image)", images: last.images?.length ? last.images : undefined });
  }
  if (!hasSystem && model.job) {
    const prefer = model.workflow ? (WORKFLOWS[model.workflow] as { prefer?: "capable" }).prefer : undefined;
    const needs = [...(request.tools?.length ? ["tools"] : []), ...(request.think === true ? ["thinking"] : [])];
    const picked = await pickLiveJobModel(model.job, serveEndpoint, prefer, needs);
    const system = applyJob(model.job, { prompt: last.content ?? "", tools: request.tools }, picked, false, model.workflow).fields.systemPrompt;
    return clean({ ...common, ...target, model: picked, messages: system === undefined ? messages : [{ role: "system", content: system }, ...messages] });
  }
  return clean({ ...common, ...target, messages });
}

function clean(fields: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined));
}

/** Run one chat turn of a virtual model through run_task. */
export async function runVirtualChat(model: VirtualModel, request: ChatRequest, options: { serveEndpoint?: string; signal?: AbortSignal } = {}): Promise<ChatAnswer> {
  let result: ToolResult;
  try {
    result = await callTool("run_task", await chatTaskArgs(model, request, options.serveEndpoint), { signal: options.signal });
  } catch (error) {
    if (error instanceof GatewayError) throw error;
    throw new GatewayError((error as Error).message, error instanceof UsageError ? 400 : 500);
  }
  const text = resultText(result);
  if (result.isError) {
    const refusal = refusalOf(text);
    throw new GatewayError(refusal.message, refusal.unavailable ? 503 : 500);
  }
  const receipt = result.structuredContent ?? {};
  const message = record(record(receipt.response)?.message);
  const picked = record(receipt.route)?.selected_model ?? receipt.selected_model ?? record(receipt.job_profile)?.model;
  const content = typeof message?.content === "string" ? message.content : splitAnswer(text).answer;
  return {
    model: typeof picked === "string" ? picked : model.name,
    content,
    ...(typeof message?.thinking === "string" && message.thinking !== "" ? { thinking: message.thinking } : {}),
    ...(Array.isArray(message?.tool_calls) && message.tool_calls.length > 0 ? { toolCalls: message.tool_calls } : {}),
    checks: Array.isArray(receipt.checks) ? receipt.checks.map(String) : [],
    doneReason: typeof record(receipt.response)?.done_reason === "string" ? String(record(receipt.response)!.done_reason) : "stop",
    metrics: record(receipt.metrics) ?? {},
    receipt,
  };
}

// ---- Protocol mapping ----

/** Ollama's done-chunk durations and counts from the managed receipt. */
function ollamaStats(metrics: Record<string, unknown>): Record<string, unknown> {
  return clean({
    total_duration: metrics.total_duration_ns,
    load_duration: metrics.load_duration_ns,
    prompt_eval_count: metrics.prompt_tokens,
    prompt_eval_duration: metrics.prompt_duration_ns,
    eval_count: metrics.output_tokens,
    eval_duration: metrics.output_duration_ns,
  });
}

/** What FreeLlama adds to a response: the real model, what to verify, what job mode filled in. */
function extension(answer: ChatAnswer): Record<string, unknown> {
  return clean({ model: answer.model, checks: answer.checks, job_profile: answer.receipt.job_profile, cached_prompt_tokens: answer.metrics.cached_prompt_tokens });
}

/** OpenAI content (a string or text/image_url parts) -> Ollama's text plus base64 images. */
export function fromOpenAiMessage(message: Record<string, unknown>): Message {
  const { content, tool_calls: toolCalls, ...rest } = message;
  const converted: Message = { ...rest, role: String(message.role ?? "user") };
  if (typeof content === "string") converted.content = content;
  else if (Array.isArray(content)) {
    const texts: string[] = [];
    const images: string[] = [];
    for (const part of content.map(record)) {
      if (part?.type === "text" && typeof part.text === "string") texts.push(part.text);
      const url = record(part?.image_url)?.url;
      if (part?.type === "image_url" && typeof url === "string") {
        const data = /^data:[^;]+;base64,(.*)$/s.exec(url);
        if (!data) throw new GatewayError("image_url must be a base64 data: URL; the gateway fetches nothing", 400);
        images.push(data[1]!);
      }
    }
    converted.content = texts.join("\n");
    if (images.length > 0) converted.images = images;
  }
  if (Array.isArray(toolCalls)) {
    converted.tool_calls = toolCalls.map((call) => {
      const fn = record(record(call)?.function) ?? {};
      let args: unknown = fn.arguments;
      if (typeof args === "string") try { args = JSON.parse(args); } catch { /* keep the text */ }
      return { function: { name: fn.name, arguments: args } };
    });
  }
  return converted;
}

/** An OpenAI chat request -> the gateway's chat request. */
export function fromOpenAiRequest(body: Record<string, unknown>): ChatRequest {
  const options: Record<string, unknown> = {};
  const copy: Array<[string, string]> = [["temperature", "temperature"], ["top_p", "top_p"], ["seed", "seed"], ["stop", "stop"],
    ["frequency_penalty", "frequency_penalty"], ["presence_penalty", "presence_penalty"]];
  for (const [from, to] of copy) if (body[from] !== undefined && body[from] !== null) options[to] = body[from];
  const maxTokens = body.max_completion_tokens ?? body.max_tokens;
  if (typeof maxTokens === "number") options.num_predict = maxTokens;
  const responseFormat = record(body.response_format);
  const format = responseFormat?.type === "json_object" ? "json"
    : responseFormat?.type === "json_schema" ? record(responseFormat.json_schema)?.schema : undefined;
  const effort = body.reasoning_effort;
  return {
    messages: (Array.isArray(body.messages) ? body.messages : []).map((message) => fromOpenAiMessage(record(message) ?? {})),
    format,
    options,
    think: typeof effort === "string" && ["low", "medium", "high"].includes(effort) ? effort : undefined,
    tools: Array.isArray(body.tools) ? body.tools : undefined,
  };
}

/** Ollama tool calls -> OpenAI's, whose arguments are a JSON string. */
function openAiToolCalls(calls: unknown[]): unknown[] {
  return calls.map((call, index) => {
    const fn = record(record(call)?.function) ?? {};
    return { id: `call_${index}`, type: "function", function: { name: fn.name, arguments: typeof fn.arguments === "string" ? fn.arguments : JSON.stringify(fn.arguments ?? {}) } };
  });
}

export function openAiCompletion(answer: ChatAnswer, requested: string, created = Math.floor(Date.now() / 1000)): Record<string, unknown> {
  const prompt = typeof answer.metrics.prompt_tokens === "number" ? answer.metrics.prompt_tokens : 0;
  const output = typeof answer.metrics.output_tokens === "number" ? answer.metrics.output_tokens : 0;
  return {
    id: `chatcmpl-fl-${created}`,
    object: "chat.completion",
    created,
    model: requested,
    choices: [{
      index: 0,
      message: clean({ role: "assistant", content: answer.content, reasoning: answer.thinking, tool_calls: answer.toolCalls ? openAiToolCalls(answer.toolCalls) : undefined }),
      // A thinking model can spend the whole budget before answering: `length` says so.
      finish_reason: answer.toolCalls ? "tool_calls" : answer.doneReason === "length" ? "length" : "stop",
    }],
    usage: { prompt_tokens: prompt, completion_tokens: output, total_tokens: prompt + output },
    freellama: extension(answer),
  };
}

// ---- HTTP ----

export type GatewayOptions = {
  host?: string;
  port?: number;
  serveEndpoint?: string;
  /** Append check: lines to the answer text, for a person reading it in a chat. */
  checksInContent?: boolean;
  log?: (line: string) => void;
};

const MAX_BODY_BYTES = 64 * 1024 * 1024;

function readBody(request: http.IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    request.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        reject(new GatewayError(`request body over ${MAX_BODY_BYTES} bytes`, 413));
        request.destroy();
      } else chunks.push(chunk);
    });
    request.on("end", () => resolve(Buffer.concat(chunks)));
    request.on("error", reject);
  });
}

function sendJson(response: http.ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

/** A browser page on another origin is refused; non-browser clients send no Origin. */
export function originAllowed(origin: string | undefined): boolean {
  if (origin === undefined) return true;
  try {
    return ["127.0.0.1", "localhost", "[::1]"].includes(new URL(origin).hostname);
  } catch {
    return false;
  }
}

/** Forward a request to serve as is, streaming both ways. */
function proxy(request: http.IncomingMessage, response: http.ServerResponse, serveEndpoint: string, body?: Buffer): void {
  const target = new URL(request.url ?? "/", serveEndpoint);
  const upstream = http.request(target, { method: request.method, headers: { ...request.headers, host: target.host } }, (reply) => {
    response.writeHead(reply.statusCode ?? 502, reply.headers);
    reply.pipe(response);
  });
  upstream.on("error", (error) => {
    if (!response.headersSent) sendJson(response, 502, { error: `freellama serve at ${serveEndpoint} did not answer: ${error.message}` });
    else response.destroy();
  });
  if (body !== undefined) upstream.end(body);
  else request.pipe(upstream);
}

async function serveJson(serveEndpoint: string, pathname: string): Promise<Record<string, unknown> | undefined> {
  try {
    const reply = await fetch(new URL(pathname, serveEndpoint), { signal: AbortSignal.timeout(5_000) });
    return reply.ok ? record(await reply.json()) : undefined;
  } catch {
    return undefined;
  }
}

function ollamaTag(model: VirtualModel, now: string): Record<string, unknown> {
  return {
    // The ollama CLI prints digest[:12]; a stable hash of the name keeps `ollama list` working.
    name: `${model.name}:latest`, model: `${model.name}:latest`, modified_at: now, size: 0, digest: createHash("sha256").update(model.name).digest("hex"),
    details: { parent_model: "", format: "freellama", family: "freellama", families: ["freellama"], parameter_size: "", quantization_level: "" },
  };
}

function ollamaShow(model: VirtualModel): Record<string, unknown> {
  return {
    license: "",
    modelfile: `# FreeLlama virtual model: ${model.title}\n# Runs on the tested pick that is installed and fits now; see \`fllama workflows\`.`,
    parameters: "",
    template: "{{ .Prompt }}",
    details: { parent_model: "", format: "freellama", family: "freellama", families: ["freellama"], parameter_size: "", quantization_level: "" },
    model_info: {},
    capabilities: ["completion", ...(model.vision ? ["vision"] : [])],
    modified_at: new Date().toISOString(),
  };
}

/** Ollama's preload request (no messages or prompt): answer at once, as Ollama does for a loaded model. */
function isLoadOnly(body: Record<string, unknown>, path: string): boolean {
  return path === "/api/chat" ? !Array.isArray(body.messages) || body.messages.length === 0 : typeof body.prompt !== "string" || body.prompt === "";
}

export async function startGateway(options: GatewayOptions = {}): Promise<{ url: string; port: number; close: () => Promise<void> }> {
  const serveEndpoint = options.serveEndpoint ?? DEFAULT_SERVE_ENDPOINT;
  const explicitEndpoint = options.serveEndpoint;
  const log = options.log ?? (() => undefined);
  const withChecks = (answer: ChatAnswer) =>
    options.checksInContent && answer.checks.length > 0 ? `${answer.content}\n\n${answer.checks.map((check) => `check: ${check}`).join("\n")}` : answer.content;

  const server = http.createServer(async (request, response) => {
    const abort = new AbortController();
    response.on("close", () => { if (!response.writableFinished) abort.abort(); });
    try {
      if (!originAllowed(request.headers.origin)) return sendJson(response, 403, { error: "origin not allowed" });
      const url = new URL(request.url ?? "/", "http://gateway");
      const path = url.pathname;
      const now = new Date().toISOString();
      // The ollama CLI's heartbeat (HEAD /) needs only a 200.
      if ((request.method === "GET" || request.method === "HEAD") && path === "/") {
        response.writeHead(200, { "content-type": "text/plain" });
        return response.end(request.method === "HEAD" ? undefined : "FreeLlama gateway is running");
      }
      if (request.method === "GET" && path === "/api/tags") {
        await ensureServe(explicitEndpoint);
        const real = await serveJson(serveEndpoint, "/api/tags");
        const models = Array.isArray(real?.models) ? real.models : [];
        return sendJson(response, 200, { models: [...virtualModels().map((model) => ollamaTag(model, now)), ...models] });
      }
      if (request.method === "GET" && path === "/v1/models") {
        await ensureServe(explicitEndpoint);
        const real = await serveJson(serveEndpoint, "/v1/models");
        const created = Math.floor(Date.now() / 1000);
        const data = [...virtualModels().map((model) => ({ id: model.name, object: "model", created, owned_by: "freellama" })), ...(Array.isArray(real?.data) ? real.data : [])];
        return sendJson(response, 200, { object: "list", data });
      }
      const handled = request.method === "POST" && ["/api/chat", "/api/generate", "/api/show", "/v1/chat/completions"].includes(path);
      if (!handled) {
        await ensureServe(explicitEndpoint);
        return proxy(request, response, serveEndpoint);
      }
      const raw = await readBody(request);
      let body: Record<string, unknown>;
      try {
        body = record(JSON.parse(raw.toString("utf8") || "{}")) ?? {};
      } catch {
        return sendJson(response, 400, { error: "request body is not JSON" });
      }
      const model = virtualModel(body.model ?? body.name);
      if (!model) {
        await ensureServe(explicitEndpoint);
        return proxy(request, response, serveEndpoint, raw);
      }
      if (path === "/api/show") return sendJson(response, 200, ollamaShow(model));
      const requested = String(body.model);
      if (path !== "/v1/chat/completions" && isLoadOnly(body, path)) {
        const done = { model: requested, created_at: now, done: true, done_reason: "load" };
        return sendJson(response, 200, path === "/api/chat" ? { ...done, message: { role: "assistant", content: "" } } : { ...done, response: "" });
      }
      const chat: ChatRequest = path === "/v1/chat/completions" ? fromOpenAiRequest(body) : {
        messages: path === "/api/chat"
          ? (body.messages as Message[])
          : [...(typeof body.system === "string" && body.system ? [{ role: "system", content: body.system }] : []),
            { role: "user", content: String(body.prompt), ...(Array.isArray(body.images) ? { images: body.images as string[] } : {}) }],
        format: body.format,
        options: record(body.options),
        think: body.think as boolean | string | undefined,
        keepAlive: body.keep_alive === undefined ? undefined : String(body.keep_alive),
        tools: Array.isArray(body.tools) ? body.tools : undefined,
      };
      const answer = await runVirtualChat(model, chat, { serveEndpoint: explicitEndpoint, signal: abort.signal });
      log(`${requested} -> ${answer.model}${answer.checks.length ? `; ${answer.checks.length} check(s)` : ""}`);
      const content = withChecks(answer);
      if (path === "/v1/chat/completions") {
        const completion = openAiCompletion({ ...answer, content }, requested);
        if (body.stream !== true) return sendJson(response, 200, completion);
        // One delta with the whole answer: managed tasks are not streamed token by token.
        const choice = (completion.choices as Array<Record<string, unknown>>)[0]!;
        const base = { id: completion.id, object: "chat.completion.chunk", created: completion.created, model: requested };
        response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
        response.write(`data: ${JSON.stringify({ ...base, choices: [{ index: 0, delta: choice.message, finish_reason: null }] })}\n\n`);
        response.write(`data: ${JSON.stringify({ ...base, choices: [{ index: 0, delta: {}, finish_reason: choice.finish_reason }], usage: completion.usage, freellama: completion.freellama })}\n\n`);
        return response.end("data: [DONE]\n\n");
      }
      const message = clean({ role: "assistant", content, thinking: answer.thinking, tool_calls: answer.toolCalls });
      const final = { model: requested, created_at: new Date().toISOString(), done: true, done_reason: answer.doneReason, ...ollamaStats(answer.metrics), freellama: extension(answer) };
      const piece = path === "/api/chat" ? { message } : clean({ response: content, thinking: answer.thinking });
      if (body.stream === false) return sendJson(response, 200, { ...final, ...piece });
      // Ollama streams NDJSON by default: the answer in one chunk, then the done chunk with stats.
      response.writeHead(200, { "content-type": "application/x-ndjson" });
      response.write(`${JSON.stringify({ model: requested, created_at: now, ...piece, done: false })}\n`);
      response.end(`${JSON.stringify(path === "/api/chat" ? { ...final, message: { role: "assistant", content: "" } } : { ...final, response: "" })}\n`);
    } catch (error) {
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
  const host = options.host ?? "127.0.0.1";
  return {
    url: `http://${host}:${port}`,
    port,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}
