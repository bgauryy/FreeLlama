// A non-streaming chat turn of a virtual model as a managed run_task: memory reservation,
// eviction, a full receipt and checks. Pick, prompts and options come from virtual.ts, the one
// owner; this file maps the turn to run_task and the answer back to Ollama's and OpenAI's shapes.
import { randomUUID } from "node:crypto";
import { UsageError } from "./args.js";
import { splitAnswer } from "./output.js";
import { callTool, resultText, type ToolResult } from "./registry.js";
import { record, withoutUndefined, type Fields } from "./util.js";
import { GatewayError, jobFields, pickVirtual, refusalError, residentContext, thinkFromEffort, type VirtualModel } from "./virtual.js";
import type { JobName } from "../local-prompts.js";

type Message = { role: string; content?: string; images?: string[]; tool_calls?: unknown[]; [key: string]: unknown };
export type ChatRequest = { messages: Message[]; format?: unknown; options?: Fields; think?: boolean | string; keepAlive?: string; tools?: unknown[] };
type ChatAnswer = {
  model: string;
  content: string;
  thinking?: string;
  toolCalls?: unknown[];
  checks: string[];
  /** Ollama's done_reason: `stop`, or `length` when the output budget ran out. */
  doneReason: string;
  metrics: Fields;
  receipt: Fields;
};

/**
 * The run_task arguments for one turn. A single user turn without a system message is the plain
 * workflow call, so job mode applies everything it does over MCP. A longer conversation without
 * one gets the workflow's system prompt on the job's pick; a caller's system message is kept.
 */
export async function chatTaskArgs(model: VirtualModel, request: ChatRequest, serveEndpoint?: string): Promise<Fields> {
  const options = { ...request.options };
  const contextTokens = typeof options.num_ctx === "number" ? options.num_ctx : undefined;
  delete options.num_ctx;
  const common = {
    endpoint: serveEndpoint,
    format: request.format,
    options: Object.keys(options).length > 0 ? options : undefined,
    think: request.think,
    keepAlive: request.keepAlive,
    tools: request.tools?.length ? request.tools : undefined,
    contextTokens,
  };
  const target = model.workflow ? { workflow: model.workflow } : model.job ? { job: model.job }
    : { task: "completion", objective: "balanced", model: await pickVirtual(model, request, serveEndpoint) };
  const messages = request.messages.filter((message) => message.role !== "system" || (message.content ?? "") !== "");
  const last = messages.at(-1);
  if (!last) throw new GatewayError("the conversation has no messages", 400);
  const hasSystem = messages.some((message) => message.role === "system");
  if (!hasSystem && messages.length === 1 && last.role === "user") {
    // An empty turn (an image alone) is left to the workflow's own instruction.
    return withoutUndefined({ ...common, ...target, prompt: last.content || undefined, images: last.images?.length ? last.images : undefined });
  }
  if (!hasSystem && model.job) {
    const picked = (await pickVirtual(model, request, serveEndpoint))!;
    const turns = messages.map((message) => ({ role: message.role, content: message.content ?? "" }));
    const system = jobFields(model as VirtualModel & { job: JobName }, picked, turns, { tools: request.tools }).systemPrompt;
    // A later turn keeps the loaded runner's window, so the conversation's prompt cache survives.
    const resident = contextTokens === undefined ? await residentContext(picked, serveEndpoint) : undefined;
    return withoutUndefined({ ...common, ...target, model: picked, contextTokens: contextTokens ?? resident, messages: system === undefined ? messages : [{ role: "system", content: system }, ...messages] });
  }
  return withoutUndefined({ ...common, ...target, messages });
}

/** Run one turn through run_task and read the answer out of its receipt. */
export async function runManagedChat(model: VirtualModel, request: ChatRequest, options: { serveEndpoint?: string; signal?: AbortSignal } = {}): Promise<ChatAnswer> {
  let result: ToolResult;
  try {
    result = await callTool("run_task", await chatTaskArgs(model, request, options.serveEndpoint), { signal: options.signal });
  } catch (error) {
    if (error instanceof GatewayError) throw error;
    throw new GatewayError((error as Error).message, error instanceof UsageError ? 400 : 500);
  }
  const text = resultText(result);
  if (result.isError) throw refusalError(text);
  const receipt = result.structuredContent ?? {};
  const response = record(receipt.response);
  const message = record(response?.message);
  const picked = record(receipt.route)?.selected_model ?? receipt.selected_model ?? record(receipt.job_profile)?.model;
  return {
    model: typeof picked === "string" ? picked : model.name,
    content: typeof message?.content === "string" ? message.content : splitAnswer(text).answer,
    ...(typeof message?.thinking === "string" && message.thinking !== "" ? { thinking: message.thinking } : {}),
    ...(Array.isArray(message?.tool_calls) && message.tool_calls.length > 0 ? { toolCalls: message.tool_calls } : {}),
    checks: Array.isArray(receipt.checks) ? receipt.checks.map(String) : [],
    doneReason: typeof response?.done_reason === "string" ? response.done_reason : "stop",
    metrics: record(receipt.metrics) ?? {},
    receipt,
  };
}

/** What FreeLlama adds to a response: the real model, what to verify, what job mode filled in. */
function extension(answer: ChatAnswer): Fields {
  return withoutUndefined({ model: answer.model, checks: answer.checks, job_profile: answer.receipt.job_profile, cached_prompt_tokens: answer.metrics.cached_prompt_tokens });
}

/** The answer text, with check: lines appended for a person reading it in a chat. */
export function answerText(answer: ChatAnswer, withChecks: boolean): string {
  return withChecks && answer.checks.length > 0 ? `${answer.content}\n\n${answer.checks.map((check) => `check: ${check}`).join("\n")}` : answer.content;
}

/** The answer as Ollama's non-streaming `/api/chat` or `/api/generate` response. */
export function ollamaResponse(answer: ChatAnswer, requested: string, path: string, content: string): Fields {
  const { metrics } = answer;
  return {
    model: requested,
    created_at: new Date().toISOString(),
    ...(path === "/api/chat"
      ? { message: withoutUndefined({ role: "assistant", content, thinking: answer.thinking, tool_calls: answer.toolCalls }) }
      : withoutUndefined({ response: content, thinking: answer.thinking })),
    done: true,
    done_reason: answer.doneReason,
    ...withoutUndefined({
      total_duration: metrics.total_duration_ns, load_duration: metrics.load_duration_ns,
      prompt_eval_count: metrics.prompt_tokens, prompt_eval_duration: metrics.prompt_duration_ns,
      eval_count: metrics.output_tokens, eval_duration: metrics.output_duration_ns,
    }),
    freellama: extension(answer),
  };
}

/** A non-streaming Ollama `/api/chat` or `/api/generate` body as a chat request. */
export function fromOllamaRequest(body: Fields, path: string): ChatRequest {
  const messages = path === "/api/chat"
    ? (Array.isArray(body.messages) ? body.messages : []) as Message[]
    : [...(typeof body.system === "string" && body.system ? [{ role: "system", content: body.system }] : []),
      { role: "user", content: String(body.prompt ?? ""), ...(Array.isArray(body.images) ? { images: body.images as string[] } : {}) }];
  return {
    messages,
    format: body.format,
    options: record(body.options),
    think: body.think as boolean | string | undefined,
    keepAlive: body.keep_alive === undefined ? undefined : String(body.keep_alive),
    tools: Array.isArray(body.tools) ? body.tools : undefined,
  };
}

/** OpenAI content (a string or text/image_url parts) -> Ollama's text plus base64 images. */
function fromOpenAiMessage(message: Fields): Message {
  const { content, tool_calls: toolCalls, ...rest } = message;
  // OpenAI's `developer` role is a system message to every local model.
  const role = message.role === "developer" ? "system" : String(message.role ?? "user");
  const converted: Message = { ...rest, role };
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

/** A non-streaming OpenAI chat request as a chat request. */
export function fromOpenAiRequest(body: Fields): ChatRequest {
  const options: Fields = {};
  for (const name of ["temperature", "top_p", "seed", "frequency_penalty", "presence_penalty"]) if (body[name] !== undefined && body[name] !== null) options[name] = body[name];
  // OpenAI allows one stop string; Ollama takes a list.
  if (body.stop !== undefined && body.stop !== null) options.stop = Array.isArray(body.stop) ? body.stop : [body.stop];
  const maxTokens = body.max_completion_tokens ?? body.max_tokens;
  if (typeof maxTokens === "number") options.num_predict = maxTokens;
  const responseFormat = record(body.response_format);
  const format = responseFormat?.type === "json_object" ? "json"
    : responseFormat?.type === "json_schema" ? record(responseFormat.json_schema)?.schema : undefined;
  return {
    messages: (Array.isArray(body.messages) ? body.messages : []).map((message) => fromOpenAiMessage(record(message) ?? {})),
    format,
    options,
    think: thinkFromEffort(body.reasoning_effort ?? record(body.reasoning)?.effort),
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

export function openAiCompletion(answer: ChatAnswer, requested: string, content = answer.content): Fields {
  const prompt = typeof answer.metrics.prompt_tokens === "number" ? answer.metrics.prompt_tokens : 0;
  const output = typeof answer.metrics.output_tokens === "number" ? answer.metrics.output_tokens : 0;
  return {
    id: `chatcmpl-${randomUUID()}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model: requested,
    choices: [{
      index: 0,
      message: withoutUndefined({ role: "assistant", content, reasoning: answer.thinking, tool_calls: answer.toolCalls ? openAiToolCalls(answer.toolCalls) : undefined }),
      // A thinking model can spend the whole budget before answering: `length` says so.
      finish_reason: answer.toolCalls ? "tool_calls" : answer.doneReason === "length" ? "length" : "stop",
    }],
    usage: { prompt_tokens: prompt, completion_tokens: output, total_tokens: prompt + output },
    freellama: extension(answer),
  };
}
