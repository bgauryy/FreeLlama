// A virtual model resolved to what Ollama itself can run: the job's pick that fits now, the
// workflow's system prompt, the documented options and think setting, and the routed context size,
// with every field the caller sent left as theirs. The gateway rewrites a request with this and
// forwards it to the matching Ollama endpoint through serve, so token streaming, tool-call
// streaming and each protocol's encoding (Ollama, OpenAI Chat and Responses, Anthropic Messages)
// are Ollama's own.
import { applyJob, pickLiveJobModel } from "../job-mode.js";
import { LOCAL_JOBS, WORKFLOWS } from "../local-prompts.js";
import { resultChecks } from "../result-checks.js";
import { callTool, resultText } from "./registry.js";
import { GatewayError, refusalOf } from "./output.js";
import type { VirtualModel } from "./gateway.js";

type Fields = Record<string, unknown>;
const record = (value: unknown): Fields | undefined => (value && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined);

export type Turn = { role: string; content: string };

/** What the caller already chose; job mode fills only what is missing. */
export type CallerChoices = { system?: boolean; options?: Fields; think?: unknown; format?: unknown; tools?: unknown[] };

export type Resolution = {
  model: string;
  /** The workflow's system prompt, when the caller sent none. */
  system?: string;
  /** A rewritten single user turn (OCR prefixes, field lists), when the workflow builds one. */
  prompt?: string;
  /** Job options the caller did not set (caller options are not repeated here). */
  options: Fields;
  think?: boolean | string;
  format?: unknown;
  /** The context window routing chose for this model and task. */
  numCtx?: number;
  job?: string;
  task: string;
};

/**
 * Resolve `model` for a conversation. One user turn without a system message is the workflow's
 * own call (its prompt rewrite applies); a longer conversation gets the system prompt only when
 * the caller sent none, as in the managed path.
 */
export async function resolveVirtual(model: VirtualModel, turns: Turn[], caller: CallerChoices, serveEndpoint?: string): Promise<Resolution> {
  const task = model.job ? LOCAL_JOBS[model.job].task : "completion";
  let picked: string;
  if (model.job) {
    const prefer = model.workflow ? (WORKFLOWS[model.workflow] as { prefer?: "capable" }).prefer : undefined;
    const needs = [...(caller.tools?.length ? ["tools"] : []), ...(caller.think === true ? ["thinking"] : [])];
    try {
      picked = await pickLiveJobModel(model.job, serveEndpoint, prefer, needs);
    } catch (error) {
      const refusal = refusalOf((error as Error).message);
      throw new GatewayError(refusal.message, refusal.unavailable ? 503 : 500);
    }
  } else {
    picked = "";
  }
  // A route preview: no inference, no reservation. It names auto's model and the context window.
  const preview = await callTool("run_task", { preview: true, task, ...(picked ? { model: picked } : { objective: "balanced" }), ...(serveEndpoint ? { endpoint: serveEndpoint } : {}) });
  if (preview.isError) {
    const refusal = refusalOf(resultText(preview));
    throw new GatewayError(refusal.message, refusal.unavailable ? 503 : 500);
  }
  const route = preview.structuredContent ?? {};
  const selected = picked || (typeof route.selected_model === "string" ? route.selected_model : "");
  if (!selected) throw new GatewayError("routing chose no model for a general chat", 503);
  const numCtx = record(route.options)?.num_ctx;
  const resolution: Resolution = { model: selected, options: {}, task, ...(typeof numCtx === "number" ? { numCtx } : {}), ...(model.job ? { job: model.job } : {}) };
  if (!model.job) return resolution;

  const last = turns.at(-1);
  const single = !caller.system && turns.length === 1 && last?.role === "user";
  let fields: ReturnType<typeof applyJob>["fields"];
  try {
    ({ fields } = applyJob(model.job, {
      // An empty turn (an image alone) lets the workflow supply its own instruction.
      prompt: last?.content || undefined,
      ...(caller.system ? { messages: turns } : {}),
      options: caller.options, think: caller.think as boolean | string | undefined, format: caller.format, tools: caller.tools,
    }, selected, false, model.workflow));
  } catch (error) {
    throw new GatewayError((error as Error).message, 400);
  }
  if (!caller.system && fields.systemPrompt !== undefined) resolution.system = fields.systemPrompt;
  // The workflow rewrote the one user turn (OCR prefix, field list, or its own instruction).
  if (single && fields.prompt !== undefined && fields.prompt !== last?.content) resolution.prompt = fields.prompt;
  for (const [key, value] of Object.entries(fields.options ?? {})) if (caller.options?.[key] === undefined) resolution.options[key] = value;
  if (caller.think === undefined && fields.think !== undefined) resolution.think = fields.think;
  if (caller.format === undefined && fields.format !== undefined) resolution.format = fields.format;
  return resolution;
}

/** Text of a message's content in any of the three shapes (string, OpenAI parts, Anthropic blocks). */
export function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => {
    const block = record(part);
    return block?.type === "text" || block?.type === "input_text" ? String(block.text ?? "") : "";
  }).join("\n");
}

const turnsOf = (messages: unknown): Turn[] =>
  (Array.isArray(messages) ? messages : []).map((message) => ({ role: String(record(message)?.role ?? "user"), content: contentText(record(message)?.content) }));

/** Replace the last user turn's text with the workflow's rewrite, keeping its other parts (images). */
function rewriteLast(messages: Fields[], prompt: string | undefined, shape: "ollama" | "openai" | "anthropic"): Fields[] {
  if (prompt === undefined || messages.length === 0) return messages;
  const last = { ...messages.at(-1)! };
  if (shape === "ollama" || typeof last.content === "string") last.content = prompt;
  else if (Array.isArray(last.content)) {
    const others = last.content.filter((part) => record(part)?.type !== "text");
    last.content = [{ type: "text", text: prompt }, ...others];
  }
  return [...messages.slice(0, -1), last];
}

export type Rewrite = { body: Fields; resolution: Resolution };

/** `/api/chat`: everything maps, the context window included. */
export async function rewriteOllamaChat(model: VirtualModel, body: Fields, serveEndpoint?: string): Promise<Rewrite> {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const turns = turnsOf(messages);
  const system = turns.some((turn) => turn.role === "system" && turn.content !== "");
  const options = record(body.options) ?? {};
  const resolution = await resolveVirtual(model, turns, { system, options, think: body.think, format: body.format, tools: body.tools as unknown[] | undefined }, serveEndpoint);
  const next = rewriteLast(messages, resolution.prompt, "ollama");
  return {
    resolution,
    body: clean({
      ...body,
      model: resolution.model,
      messages: resolution.system ? [{ role: "system", content: resolution.system }, ...next] : next,
      options: { ...(resolution.numCtx && options.num_ctx === undefined ? { num_ctx: resolution.numCtx } : {}), ...resolution.options, ...options },
      think: body.think ?? resolution.think,
      format: body.format ?? resolution.format,
    }),
  };
}

/** `/api/generate`: the prompt and `system` field carry the turn. */
export async function rewriteOllamaGenerate(model: VirtualModel, body: Fields, serveEndpoint?: string): Promise<Rewrite> {
  const system = typeof body.system === "string" && body.system !== "";
  const options = record(body.options) ?? {};
  const resolution = await resolveVirtual(model, [{ role: "user", content: String(body.prompt ?? "") }], { system, options, think: body.think, format: body.format }, serveEndpoint);
  return {
    resolution,
    body: clean({
      ...body,
      model: resolution.model,
      prompt: resolution.prompt ?? body.prompt,
      system: system ? body.system : resolution.system,
      options: { ...(resolution.numCtx && options.num_ctx === undefined ? { num_ctx: resolution.numCtx } : {}), ...resolution.options, ...options },
      think: body.think ?? resolution.think,
      format: body.format ?? resolution.format,
    }),
  };
}

/** Sampling fields the OpenAI and Anthropic shapes name directly, from the job's options. */
function sampling(options: Fields, body: Fields, names: Array<[string, string]>): Fields {
  return Object.fromEntries(names.filter(([option, field]) => options[option] !== undefined && body[field] === undefined).map(([option, field]) => [field, options[option]]));
}

/**
 * `/v1/chat/completions`. Ollama's OpenAI layer has no context-size field, so the model's default
 * window applies; thinking off is `reasoning_effort: "none"`.
 */
export async function rewriteOpenAiChat(model: VirtualModel, body: Fields, serveEndpoint?: string): Promise<Rewrite> {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const turns = turnsOf(messages);
  const system = turns.some((turn) => (turn.role === "system" || turn.role === "developer") && turn.content !== "");
  const resolution = await resolveVirtual(model, turns, { system, think: body.reasoning_effort ?? record(body.reasoning)?.effort, tools: body.tools as unknown[] | undefined }, serveEndpoint);
  const next = rewriteLast(messages, resolution.prompt, "openai");
  return {
    resolution,
    body: clean({
      ...body,
      model: resolution.model,
      messages: resolution.system ? [{ role: "system", content: resolution.system }, ...next] : next,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"], ["presence_penalty", "presence_penalty"], ["frequency_penalty", "frequency_penalty"], ["seed", "seed"]]),
      ...(resolution.think === false && body.reasoning_effort === undefined && body.reasoning === undefined ? { reasoning_effort: "none" } : {}),
      ...(typeof resolution.think === "string" && body.reasoning_effort === undefined && body.reasoning === undefined ? { reasoning_effort: resolution.think } : {}),
    }),
  };
}

/** `/v1/messages` (Anthropic). The system prompt is the `system` field; thinking off is `thinking.type: "disabled"`. */
export async function rewriteAnthropic(model: VirtualModel, body: Fields, serveEndpoint?: string): Promise<Rewrite> {
  const messages = (Array.isArray(body.messages) ? body.messages : []) as Fields[];
  const system = body.system !== undefined && contentText(body.system) !== "";
  const resolution = await resolveVirtual(model, turnsOf(messages), { system, think: body.thinking, tools: body.tools as unknown[] | undefined }, serveEndpoint);
  return {
    resolution,
    body: clean({
      ...body,
      model: resolution.model,
      messages: rewriteLast(messages, resolution.prompt, "anthropic"),
      system: system ? body.system : resolution.system,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"], ["top_k", "top_k"]]),
      ...(resolution.think === false && body.thinking === undefined ? { thinking: { type: "disabled" } } : {}),
    }),
  };
}

/** `/v1/responses` (OpenAI Responses). The system prompt is `instructions`; Ollama accepts `think`. */
export async function rewriteResponses(model: VirtualModel, body: Fields, serveEndpoint?: string): Promise<Rewrite> {
  const input = typeof body.input === "string" ? [{ role: "user", content: body.input }] : Array.isArray(body.input) ? body.input as Fields[] : [];
  const turns = turnsOf(input.filter((item) => record(item)?.role !== undefined));
  const system = (typeof body.instructions === "string" && body.instructions !== "") || turns.some((turn) => turn.role === "system" || turn.role === "developer");
  const resolution = await resolveVirtual(model, turns, { system, think: body.think ?? record(body.reasoning)?.effort, tools: body.tools as unknown[] | undefined }, serveEndpoint);
  const single = typeof body.input === "string";
  return {
    resolution,
    body: clean({
      ...body,
      model: resolution.model,
      input: single && resolution.prompt !== undefined ? resolution.prompt : body.input,
      instructions: system ? body.instructions : resolution.system,
      ...sampling(resolution.options, body, [["temperature", "temperature"], ["top_p", "top_p"]]),
      ...(resolution.think !== undefined && body.think === undefined && body.reasoning === undefined ? { think: resolution.think } : {}),
    }),
  };
}

/** The `check:` lines job mode's result checks give for a finished answer. */
export function streamChecks(resolution: Resolution, answer: string, tools?: unknown[]): string[] {
  return resultChecks({ job: resolution.job, task: resolution.task, answer, structured: { response: { message: { role: "assistant", content: answer } } }, tools });
}

function clean(fields: Fields): Fields {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined && !(value && typeof value === "object" && !Array.isArray(value) && Object.keys(value).length === 0)));
}
