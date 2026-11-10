// FreeLlama virtual models: `freellama/<workflow>`, `freellama/<job>` and `freellama/auto`, and the
// one place that resolves them to what Ollama can run: the job's pick that fits now, the workflow's
// system prompt, the documented options and think setting, and the routed context size. Every
// field the caller sent stays theirs. The gateway rewrites a request with a resolution and forwards
// it to Ollama's own endpoint through serve, so token and tool-call streaming and each protocol's
// encoding (Ollama, OpenAI Chat and Responses, Anthropic Messages) are Ollama's.
import { applyJob, neededCapabilities, pickLiveJobModel } from "../job-mode.js";
import { LOCAL_JOBS, RUN_TASK_JOBS, WORKFLOWS, WORKFLOW_NAMES, type JobName, type WorkflowName } from "../local-prompts.js";
import { resultChecks } from "../result-checks.js";
import { UsageError } from "./args.js";
import { refusalOf } from "./output.js";
import { callTool, resultText } from "./registry.js";
import { record, withoutEmpty, type Fields } from "./util.js";

export const VIRTUAL_PREFIX = "freellama/";
/** Decision and embedding jobs need fields a chat cannot carry; `fllama run` serves them. */
const CHAT_TASKS = new Set(["coding", "completion", "vision"]);

export type VirtualModel = { name: string; title: string; job?: JobName; workflow?: WorkflowName; vision: boolean };

/** A refusal or failure with the HTTP status a gateway client should see. */
export class GatewayError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

/** A tool refusal as a GatewayError: 503 when waiting can fix it, else 500. */
export function refusalError(text: string): GatewayError {
  const refusal = refusalOf(text);
  return new GatewayError(refusal.message, refusal.unavailable ? 503 : 500);
}

/** The chat-shaped workflows and jobs, plus `auto`. */
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

/** `freellama/fix-bug` or `freellama/fix-bug:latest` (the ollama CLI adds the tag) -> its virtual model. */
export function virtualModel(name: unknown): VirtualModel | undefined {
  if (typeof name !== "string" || !name.toLowerCase().startsWith(VIRTUAL_PREFIX)) return undefined;
  const bare = name.toLowerCase().replace(/:latest$/, "");
  return virtualModels().find((model) => model.name === bare);
}

type Turn = { role: string; content: string };
/** What the caller already chose; job mode fills only what is missing. */
type CallerChoices = { system?: boolean; options?: Fields; think?: boolean | string; format?: unknown; tools?: unknown[] };

export type Resolution = {
  model: string;
  /** The workflow's system prompt, when the caller sent none. */
  system?: string;
  /** The one user turn as the workflow rewrites it (OCR prefix, field list, its own instruction). */
  prompt?: string;
  /** Job options the caller did not set. */
  options: Fields;
  think?: boolean | string;
  format?: unknown;
  /** The context window routing chose for this model and task. */
  numCtx?: number;
  job?: JobName;
  task: string;
};

/** The job's tested pick that is installed and fits now; undefined for `auto` (routing picks). */
export async function pickVirtual(model: VirtualModel, caller: Pick<CallerChoices, "tools" | "think">, serveEndpoint?: string): Promise<string | undefined> {
  if (!model.job) return undefined;
  const prefer = model.workflow ? (WORKFLOWS[model.workflow] as { prefer?: "capable" }).prefer : undefined;
  try {
    return await pickLiveJobModel(model.job, serveEndpoint, prefer, neededCapabilities(caller));
  } catch (error) {
    throw refusalError((error as Error).message);
  }
}

/** Job mode's fields for one turn on `picked`: the single source of prompts, options and think. */
export function jobFields(model: VirtualModel & { job: JobName }, picked: string, turns: Turn[], caller: CallerChoices): ReturnType<typeof applyJob>["fields"] {
  try {
    return applyJob(model.job, {
      // An empty turn (an image alone) lets the workflow supply its own instruction.
      prompt: turns.at(-1)?.content || undefined,
      ...(caller.system ? { messages: turns } : {}),
      options: caller.options, think: caller.think, format: caller.format, tools: caller.tools,
    }, picked, false, model.workflow).fields;
  } catch (error) {
    throw new GatewayError((error as Error).message, 400);
  }
}

/**
 * Resolve `model` for a conversation. One user turn without a system message is the workflow's
 * own call, so its prompt rewrite applies. A longer conversation gets the workflow's system prompt
 * only when the caller sent none.
 */
export async function resolveVirtual(model: VirtualModel, turns: Turn[], caller: CallerChoices, serveEndpoint?: string): Promise<Resolution> {
  const task = model.job ? LOCAL_JOBS[model.job].task : "completion";
  const picked = await pickVirtual(model, caller, serveEndpoint);
  // A route preview runs nothing and reserves nothing. It names auto's model and the context window.
  let preview;
  try {
    preview = await callTool("run_task", { preview: true, task, ...(picked ? { model: picked } : { objective: "balanced" }), ...(serveEndpoint ? { endpoint: serveEndpoint } : {}) });
  } catch (error) {
    throw new GatewayError((error as Error).message, error instanceof UsageError ? 400 : 500);
  }
  if (preview.isError) throw refusalError(resultText(preview));
  const route = preview.structuredContent ?? {};
  const selected = picked ?? (typeof route.selected_model === "string" ? route.selected_model : undefined);
  if (!selected) throw new GatewayError("routing chose no model for a general chat", 503);
  const numCtx = record(route.options)?.num_ctx;
  const resolution: Resolution = { model: selected, options: {}, task, ...(typeof numCtx === "number" ? { numCtx } : {}) };
  if (!model.job) return resolution;

  resolution.job = model.job;
  const fields = jobFields(model as VirtualModel & { job: JobName }, selected, turns, caller);
  const last = turns.at(-1);
  if (!caller.system && fields.systemPrompt !== undefined) resolution.system = fields.systemPrompt;
  if (!caller.system && turns.length === 1 && last?.role === "user" && fields.prompt !== undefined && fields.prompt !== last.content) resolution.prompt = fields.prompt;
  for (const [key, value] of Object.entries(fields.options ?? {})) if (caller.options?.[key] === undefined) resolution.options[key] = value;
  if (caller.think === undefined && fields.think !== undefined) resolution.think = fields.think;
  if (caller.format === undefined && fields.format !== undefined) resolution.format = fields.format;
  return resolution;
}

/** Text of a message's content in any shape: a string, OpenAI parts or Anthropic blocks. */
function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => {
    const block = record(part);
    return block?.type === "text" || block?.type === "input_text" ? String(block.text ?? "") : "";
  }).join("\n");
}

const turnsOf = (messages: unknown): Turn[] =>
  (Array.isArray(messages) ? messages : []).map((message) => ({ role: String(record(message)?.role ?? "user"), content: contentText(record(message)?.content) }));

const SYSTEM_ROLES = new Set(["system", "developer"]);
const hasSystem = (turns: Turn[]) => turns.some((turn) => SYSTEM_ROLES.has(turn.role) && turn.content !== "");

/** Replace the last turn's text with the workflow's rewrite, keeping its other parts (images). */
function rewriteLast(messages: Fields[], prompt: string | undefined): Fields[] {
  if (prompt === undefined || messages.length === 0) return messages;
  const last = { ...messages.at(-1)! };
  if (Array.isArray(last.content)) {
    const others = last.content.filter((part) => !["text", "input_text"].includes(String(record(part)?.type)));
    last.content = [{ type: "text", text: prompt }, ...others];
  } else last.content = prompt;
  return [...messages.slice(0, -1), last];
}

/** Sampling fields a protocol names directly, filled from the job's options the caller left unset. */
function sampling(options: Fields, body: Fields, names: Array<[option: string, field: string]>): Fields {
  return Object.fromEntries(names.filter(([option, field]) => options[option] !== undefined && body[field] === undefined).map(([option, field]) => [field, options[option]]));
}

/** OpenAI's reasoning effort as Ollama's think: "none" is off, a level stays a level. */
export function thinkFromEffort(effort: unknown): boolean | string | undefined {
  if (effort === "none") return false;
  return typeof effort === "string" && effort !== "" ? effort : undefined;
}

/** Anthropic's `thinking` object as Ollama's think. */
function thinkFromAnthropic(thinking: unknown): boolean | undefined {
  const type = record(thinking)?.type;
  return type === "enabled" ? true : type === "disabled" ? false : undefined;
}

/** Ollama's `format` ("json" or a schema) as OpenAI Chat's `response_format`. */
function responseFormat(format: unknown): Fields | undefined {
  if (format === "json") return { type: "json_object" };
  return record(format) ? { type: "json_schema", json_schema: { name: "output", schema: format } } : undefined;
}

export type Rewrite = { body: Fields; resolution: Resolution };
type Rewriter = (model: VirtualModel, body: Fields, serveEndpoint?: string) => Promise<Rewrite>;

/** Ollama's options with the routed context window and the job's values under the caller's. */
function ollamaOptions(resolution: Resolution, options: Fields): Fields {
  return { ...(resolution.numCtx !== undefined && options.num_ctx === undefined ? { num_ctx: resolution.numCtx } : {}), ...resolution.options, ...options };
}

/** `/api/chat`: everything maps, the context window included. */
const rewriteOllamaChat: Rewriter = async (model, body, serveEndpoint) => {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const turns = turnsOf(messages);
  const options = record(body.options) ?? {};
  const resolution = await resolveVirtual(model, turns, { system: hasSystem(turns), options, think: body.think as boolean | string | undefined, format: body.format, tools: body.tools as unknown[] | undefined }, serveEndpoint);
  const next = rewriteLast(messages, resolution.prompt);
  return {
    resolution,
    body: withoutEmpty({
      ...body,
      model: resolution.model,
      messages: resolution.system ? [{ role: "system", content: resolution.system }, ...next] : next,
      options: ollamaOptions(resolution, options),
      think: body.think ?? resolution.think,
      format: body.format ?? resolution.format,
    }),
  };
};

/** `/api/generate`: `prompt` and `system` carry the turn. */
const rewriteOllamaGenerate: Rewriter = async (model, body, serveEndpoint) => {
  const system = typeof body.system === "string" && body.system !== "";
  const options = record(body.options) ?? {};
  const resolution = await resolveVirtual(model, [{ role: "user", content: String(body.prompt ?? "") }], { system, options, think: body.think as boolean | string | undefined, format: body.format }, serveEndpoint);
  return {
    resolution,
    body: withoutEmpty({
      ...body,
      model: resolution.model,
      prompt: resolution.prompt ?? body.prompt,
      system: system ? body.system : resolution.system,
      options: ollamaOptions(resolution, options),
      think: body.think ?? resolution.think,
      format: body.format ?? resolution.format,
    }),
  };
};

/**
 * `/v1/chat/completions`. Ollama's OpenAI layer has no context-size field, so the model's default
 * window applies. Thinking off is `reasoning_effort: "none"`; a JSON workflow's format is
 * `response_format`.
 */
const rewriteOpenAiChat: Rewriter = async (model, body, serveEndpoint) => {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const turns = turnsOf(messages);
  const effortSet = body.reasoning_effort !== undefined || body.reasoning !== undefined;
  const resolution = await resolveVirtual(model, turns, {
    system: hasSystem(turns), think: thinkFromEffort(body.reasoning_effort ?? record(body.reasoning)?.effort),
    format: body.response_format, tools: body.tools as unknown[] | undefined,
  }, serveEndpoint);
  const next = rewriteLast(messages, resolution.prompt);
  const think = resolution.think;
  return {
    resolution,
    body: withoutEmpty({
      ...body,
      model: resolution.model,
      messages: resolution.system ? [{ role: "system", content: resolution.system }, ...next] : next,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"], ["presence_penalty", "presence_penalty"], ["frequency_penalty", "frequency_penalty"], ["seed", "seed"]]),
      ...(!effortSet && think !== undefined && think !== true ? { reasoning_effort: think === false ? "none" : think } : {}),
      ...(body.response_format === undefined ? { response_format: responseFormat(resolution.format) } : {}),
    }),
  };
};

/**
 * `/v1/messages` (Anthropic). The system prompt is `system`; thinking off is
 * `thinking.type: "disabled"`. Ollama documents no output-format field here, so a JSON workflow's
 * format is not applied.
 */
const rewriteAnthropic: Rewriter = async (model, body, serveEndpoint) => {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const system = body.system !== undefined && contentText(body.system) !== "";
  const resolution = await resolveVirtual(model, turnsOf(messages), { system, think: thinkFromAnthropic(body.thinking), tools: body.tools as unknown[] | undefined }, serveEndpoint);
  return {
    resolution,
    body: withoutEmpty({
      ...body,
      model: resolution.model,
      messages: rewriteLast(messages, resolution.prompt),
      system: system ? body.system : resolution.system,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"], ["top_k", "top_k"]]),
      ...(resolution.think === false && body.thinking === undefined ? { thinking: { type: "disabled" } } : {}),
    }),
  };
};

/** `/v1/responses` (OpenAI Responses). The system prompt is `instructions`; Ollama accepts `think`. */
const rewriteResponses: Rewriter = async (model, body, serveEndpoint) => {
  const items = typeof body.input === "string" ? [{ role: "user", content: body.input }] : Array.isArray(body.input) ? body.input as Fields[] : [];
  const turns = turnsOf(items.filter((item) => record(item)?.role !== undefined));
  const system = (typeof body.instructions === "string" && body.instructions !== "") || hasSystem(turns);
  const textFormat = record(body.text)?.format;
  const effortSet = body.think !== undefined || body.reasoning !== undefined;
  const resolution = await resolveVirtual(model, turns, {
    system, think: (body.think as boolean | string | undefined) ?? thinkFromEffort(record(body.reasoning)?.effort), format: textFormat, tools: body.tools as unknown[] | undefined,
  }, serveEndpoint);
  const format = textFormat === undefined ? responseFormat(resolution.format) : undefined;
  return {
    resolution,
    body: withoutEmpty({
      ...body,
      model: resolution.model,
      input: typeof body.input === "string" ? resolution.prompt ?? body.input : rewriteLast(items, resolution.prompt),
      instructions: system ? body.instructions : resolution.system,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"]]),
      ...(!effortSet && resolution.think !== undefined ? { think: resolution.think } : {}),
      // Responses puts the schema beside `type`, not under `json_schema`.
      ...(format ? { text: { ...record(body.text), format: format.type === "json_schema" ? { type: "json_schema", ...record(format.json_schema) } : format } } : {}),
    }),
  };
};

/** The rewriter for each path a virtual model is forwarded on. */
export const REWRITERS: Record<string, Rewriter> = {
  "/api/chat": rewriteOllamaChat, "/api/generate": rewriteOllamaGenerate, "/v1/chat/completions": rewriteOpenAiChat,
  "/v1/messages": rewriteAnthropic, "/v1/responses": rewriteResponses,
};

/** The `check:` lines job mode's result checks give for a finished streamed answer. */
export function streamChecks(resolution: Resolution, answer: string, tools?: unknown[]): string[] {
  return resultChecks({ job: resolution.job, task: resolution.task, answer, structured: { response: { message: { role: "assistant", content: answer } } }, tools });
}
