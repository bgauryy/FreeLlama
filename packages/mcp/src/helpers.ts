// Shared MCP result helpers, Zod parameter schemas, the confidence gate, and payload trimming.
import { OLLAMA_MISSING, ollamaInstalled } from "./ollama-presence.js";
import * as z from "zod";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS, DEFAULT_SERVE_ENDPOINT } from "./config.js";
import { modelNames, seconds } from "./job-progress.js";
import { nearestText, type NearestResult } from "./nearest.js";

export async function ollamaFetch(
  endpoint: string | undefined,
  path: string,
  init: {
    method?: string;
    body?: unknown;
    timeoutMs?: number;
    parse?: "json" | "ndjson";
    /** MCP request cancellation; aborts the HTTP call so Ollama stops the work too. */
    signal?: AbortSignal;
    /** ndjson only: called per parsed line as it arrives (pull progress). */
    onLine?: (line: Record<string, unknown>) => void;
  } = {},
): Promise<unknown> {
  const base = (endpoint ?? DEFAULT_OLLAMA_ENDPOINT).replace(/\/$/, "");
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(),
    init.timeoutMs ?? DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000,
  );
  const signal = init.signal ? AbortSignal.any([controller.signal, init.signal]) : controller.signal;
  try {
    const response = await fetch(`${base}${path}`, {
      method: init.method ?? "GET",
      headers: init.body ? { "content-type": "application/json" } : undefined,
      body: init.body ? JSON.stringify(init.body) : undefined,
      signal,
    });
    const text = init.onLine && response.ok && response.body
      ? await readNdjson(response.body, init.onLine)
      : await response.text();
    if (!response.ok) {
      throw new Error(`Ollama ${init.method ?? "GET"} ${path} -> HTTP ${response.status}: ${text}`);
    }
    if (!text) return {};
    if (init.parse === "ndjson") return text;
    return JSON.parse(text);
  } finally {
    clearTimeout(timeout);
  }
}

async function readNdjson(
  body: ReadableStream<Uint8Array>,
  onLine: (line: Record<string, unknown>) => void,
): Promise<string> {
  const decoder = new TextDecoder();
  let text = "";
  let pending = "";
  for await (const chunk of body as unknown as AsyncIterable<Uint8Array>) {
    const piece = decoder.decode(chunk, { stream: true });
    text += piece;
    pending += piece;
    const lines = pending.split("\n");
    pending = lines.pop() ?? "";
    for (const line of lines) {
      try {
        const parsed = JSON.parse(line) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) onLine(parsed as Record<string, unknown>);
      } catch {
        // A partial or non-JSON line is summarized from the full text afterwards.
      }
    }
  }
  return text + decoder.decode();
}

// Self-evident params carry no `.describe()`: the name says it, the default is in the server
// instructions, and every description is re-sent on every request. Only params whose BEHAVIOUR
// isn't obvious from the name keep one.
export const endpointParam = z.string().min(1).optional();
export const ollamaEndpointParam = z.string().min(1).optional();
export const TASK_KINDS = [
  "completion",
  "coding",
  "code_repair",
  "tools",
  "browser",
  "vision",
  "embedding",
  "long_context",
  "decision",
] as const;
export const taskParam = z.enum(TASK_KINDS).default("completion");
export const objectiveParam = z
  .enum(["fastest", "balanced", "quality"])
  .optional()
  .describe("quality needs a task policy or an explicit model");
export const executionPreferenceParam = z
  .enum(["auto", "prefer_cpu", "prefer_gpu"])
  .optional();
export const minPlacementEvidenceParam = z
  .enum(["configured", "observed"])
  .describe("observed: refuse unless /api/ps proves where the model runs")
  .optional();
// Router grades only "low" | "medium"; the Rust core enforces the floor (`enforce_min_confidence`).
export const minConfidenceParam = z
  .enum(["low", "medium"])
  .optional()
  .describe("refuse a route graded lower; medium needs a task policy plus a benchmark");

export const REQUIRED_CAPABILITIES = [
  "completion",
  "tools",
  "vision",
  "audio",
  "thinking",
  "embedding",
  "decision",
  "speech",
] as const;
export const requiredCapabilitiesParam = z
  .array(z.enum(REQUIRED_CAPABILITIES))
  .max(REQUIRED_CAPABILITIES.length)
  .optional();

export const systemPromptParam = z.string().optional()
  .describe("Local model instructions, sent verbatim.");
// One chat message; each array sets its own length bound (zod 4 accumulates `.min` checks).
export const messageParam = z.object({
  role: z.enum(["system", "user", "assistant", "tool"]),
  content: z.string().optional(),
}).loose();
export const messagesParam = z.array(messageParam).min(1).optional();
export const localToolsParam = z.array(z.object({
  type: z.literal("function"),
  function: z.object({
    name: z.string().min(1),
    description: z.string().optional(),
    parameters: z.record(z.string(), z.unknown()).optional(),
  }).loose(),
}).loose()).min(1).optional();

/** Decision state: the text or JSON the questions are about. */
export const stateParam = z.union([z.string().min(1), z.record(z.string(), z.unknown()), z.array(z.unknown()).min(1)]).optional();
/** Decision questions, answered in order: noul = probability of true, choice = one option, score = a 0-based level. */
export const questionsParam = z.record(
  z.string().min(1).max(100),
  z.object({
    type: z.enum(["noul", "choice", "score"]),
    instructions: z.union([z.string().min(1), z.record(z.string(), z.unknown()), z.array(z.unknown())]),
    criteria: z.union([z.record(z.string(), z.string().nullable()), z.array(z.string()).min(2).max(10)]).optional(),
  }).strict(),
).optional();
/** Paths the server reads into the task, so the caller does not paste file content. */
export const filesParam = z.array(z.string().min(1)).min(1).max(2_000).optional();

const embeddingItemParam = z.union([
  z.string().min(1),
  z.object({ text: z.string().optional(), image: z.string().min(1).optional(), audio: z.string().min(1).optional() })
    .strict().refine((value) => value.text !== undefined || value.image !== undefined || value.audio !== undefined, "embedding input needs text, image or audio"),
]);
/** Ollama embedding inputs can be text or explicit modality objects, singly or mixed. */
export const embeddingInputParam = z.union([embeddingItemParam, z.array(embeddingItemParam).min(1)]).optional();

/** Only explicitly supplied instructions become a system message; existing messages stay intact. */
export function taskMessages({ systemPrompt, messages, prompt, images }: {
  systemPrompt?: string;
  messages?: Record<string, unknown>[];
  prompt?: string;
  images?: string[];
}): Record<string, unknown>[] {
  if (systemPrompt === undefined) return messages ?? [];
  const conversation = messages ?? (prompt === undefined ? [] : [{
    role: "user", content: prompt, ...(images === undefined ? {} : { images }),
  }]);
  return [{ role: "system", content: systemPrompt }, ...conversation];
}

// Full per-item shape, checked in run_task by `parseBatchTask` (strict: a misspelled or mistyped
// field fails the call). It is never sent to the model; `batchItemParam` below is.
const batchTaskParam = z.object({
  task: taskParam.removeDefault().optional(),
  objective: objectiveParam,
  model: z.string().min(1).optional(),
  sessionId: z.string().uuid().optional(),
  scopeId: z.string().uuid().optional(),
  scopeRevision: z.number().int().nonnegative().optional(),
  contextTokens: z.number().int().positive().optional(),
  executionPreference: executionPreferenceParam,
  minPlacementEvidence: minPlacementEvidenceParam,
  requiredCapabilities: requiredCapabilitiesParam,
  priority: z.enum(["interactive", "normal", "background"]).optional(),
  maxWaitSeconds: z.number().int().positive().optional(),
  timeoutSeconds: z.number().int().positive().optional(),
  stopOnRepetition: z.boolean().optional(),
  prompt: z.string().min(1).optional(),
  systemPrompt: systemPromptParam,
  messages: messagesParam,
  images: z.array(z.string().min(1)).min(1).optional(),
  input: embeddingInputParam,
  state: stateParam,
  questions: z.record(z.string(), z.record(z.string(), z.unknown())).optional(),
  files: filesParam,
  tools: localToolsParam,
  keepAlive: z.string().min(1).optional(),
  format: z.union([z.literal("json"), z.record(z.string(), z.unknown())]).optional(),
  think: z.union([z.boolean(), z.string().min(1)]).optional(),
  options: z.record(z.string(), z.unknown()).optional(),
  logprobs: z.boolean().optional(),
  topLogprobs: z.number().int().nonnegative().optional(),
  minConfidence: minConfidenceParam,
}).strict();

/** Field names a `tasks[].task` item accepts; derived from the schema so hints cannot drift. */
export const BATCH_TASK_FIELDS: ReadonlySet<string> = new Set(Object.keys(batchTaskParam.shape));

export type BatchTask = z.infer<typeof batchTaskParam>;
export type BatchItem = { id: string; independent: true; task: BatchTask };

// What the model sees: one loose task object per item, described in a line. Every field is
// validated against the full strict shape by `parseBatchTask`, so the item schema need not repeat
// the run_task fields an agent already sees at the top level (it cost about 1.5 KB per session).
export const batchItemParam = z.object({
  id: z.string().min(1),
  independent: z.literal(true),
  // The fields that usually differ between items, named so an agent sees them; any other run_task
  // field is accepted too, and parseBatchTask checks every one strictly.
  task: z.object({
    ...Object.fromEntries(["prompt", "messages", "state", "input", "files", "images", "model", "task", "questions", "options", "keepAlive", "format", "think"]
      .map((name) => [name, z.unknown().optional()])),
    // Typed where an agent writes structure the local model reads: instructions and tool descriptors.
    systemPrompt: systemPromptParam,
    tools: localToolsParam,
  }).catchall(z.unknown()),
}).strict();

/** Check one batch item's `task` against the full strict shape; the error names the item. */
export function parseBatchTask(item: { id: string; task: Record<string, unknown> }): BatchItem["task"] {
  const parsed = batchTaskParam.safeParse(item.task);
  if (!parsed.success) throw new Error(`batch item ${item.id}: ${parsed.error.issues.map((issue) =>
    `${issue.path.length ? `${issue.path.join(".")}: ` : ""}${issue.message}`).join("; ")}`);
  return parsed.data;
}

// Output schemas describe FreeLlama-owned decision envelopes while leaving Ollama-owned payloads
// open. This gives a client useful validation without making a newly-added upstream response field
// into a protocol failure: a strict schema once turned an undocumented /api/show `requires` field
// into a hard McpError.
export const objectResultSchema = z.object({}).loose();

type ExternalCost = { model: string; input: number; output: number };

/** Optional operator-owned rate card; partial or invalid configuration fails at server startup. */
export function configuredExternalCost(env: NodeJS.ProcessEnv = process.env): ExternalCost | undefined {
  const model = env.FREELLAMA_EXTERNAL_COST_MODEL;
  const inputRaw = env.FREELLAMA_EXTERNAL_COST_INPUT_USD_PER_M;
  const outputRaw = env.FREELLAMA_EXTERNAL_COST_OUTPUT_USD_PER_M;
  if (model === undefined && inputRaw === undefined && outputRaw === undefined) return undefined;
  const input = Number(inputRaw);
  const output = Number(outputRaw);
  if (!model?.trim() || !Number.isFinite(input) || input < 0 || !Number.isFinite(output) || output < 0) {
    throw new Error(
      "External cost telemetry requires FREELLAMA_EXTERNAL_COST_MODEL plus non-negative " +
      "FREELLAMA_EXTERNAL_COST_INPUT_USD_PER_M and FREELLAMA_EXTERNAL_COST_OUTPUT_USD_PER_M.",
    );
  }
  return { model, input, output };
}

type TokenUsage = { inputTokens: number | null; outputTokens: number | null; totalDurationNs?: number | null };

/**
 * Observed local token counts plus an opt-in, caller-configured external equivalent. Tokenizers,
 * cached-input discounts, retries, electricity, and hardware amortization are deliberately not
 * invented here, so this receipt is a reproducible comparison input rather than a false bill.
 */
export function costTelemetry(usage: TokenUsage, externalCost?: ExternalCost) {
  const local = {
    inputTokens: usage.inputTokens,
    outputTokens: usage.outputTokens,
    totalTokens: usage.inputTokens === null || usage.outputTokens === null ? null : usage.inputTokens + usage.outputTokens,
    totalDurationNs: usage.totalDurationNs ?? null,
  };
  if (!externalCost || usage.inputTokens === null || usage.outputTokens === null) {
    return {
      local,
      externalEquivalent: null,
      note: externalCost
        ? "External equivalent unavailable because Ollama did not report both input and output token counts."
        : "No external rate card supplied; local usage is observed, but avoided external cost is not estimated.",
    };
  }
  const inputUsd = (usage.inputTokens * externalCost.input) / 1_000_000;
  const outputUsd = (usage.outputTokens * externalCost.output) / 1_000_000;
  return {
    local,
    externalEquivalent: {
      model: externalCost.model,
      currency: "USD",
      inputUsdPerMillion: externalCost.input,
      outputUsdPerMillion: externalCost.output,
      inputUsd,
      outputUsd,
      totalUsd: inputUsd + outputUsd,
      assumption: "Same input/output token counts at the configured external rate; excludes cached-input pricing, external reasoning tokens, retries, local energy, and hardware cost.",
    },
    note: "External equivalent is a configured comparison estimate, not an observed provider bill or net local cost.",
  };
}

export function withRequiredCapability<T extends string>(
  required: readonly T[] | undefined,
  capability: T,
): T[] {
  return [...new Set([...(required ?? []), capability])];
}

/** Largest JSON text fallback; beyond it the text points at structuredContent. */
export const RESULT_TEXT_LIMIT = 4_000;

/**
 * The text of a result with no explicit `summary`. MCP says a tool returning structuredContent
 * SHOULD also return its serialized JSON as text, and text-only clients otherwise see no data.
 * The JSON is compact and capped; an oversized one is cut with a note that the whole object is in
 * structuredContent. Views that are routinely large render their own text (result-text.ts).
 */
function resultSummary(value: Record<string, unknown>): string {
  const explicit = value.summary;
  if (typeof explicit === "string" && explicit.trim()) return explicit.trim();
  const json = JSON.stringify(value);
  if (json.length <= RESULT_TEXT_LIMIT) return json;
  return (
    `${json.slice(0, RESULT_TEXT_LIMIT)}\n[freellama: JSON truncated at ${RESULT_TEXT_LIMIT} of ${json.length} characters; ` +
    "the complete object is in structuredContent]"
  );
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}

/**
 * The answer of one managed execution, as text. Many MCP clients forward only TextContent to the
 * model, so a key list here meant the delegated answer never reached the caller at all.
 */
export function taskAnswerText(payload: Record<string, unknown>): string | undefined {
  const response = record(payload.response);
  if (!response) return undefined;
  const model = record(payload.route)?.selected_model ?? payload.selected_model ?? response.model;
  const label = [typeof model === "string" && model ? model : "", lifecycleClause(payload)].filter(Boolean).join(" · ");
  const header = label ? `[${label}] ` : "";
  const message = record(response.message);
  if (message) {
    const parts: string[] = [];
    if (typeof message.content === "string" && message.content.trim()) parts.push(answerContent(message.content, payload));
    if (Array.isArray(message.tool_calls) && message.tool_calls.length > 0) {
      parts.push(`tool_calls: ${JSON.stringify(message.tool_calls)}`);
    }
    if (parts.length === 0) {
      // A thinking model can spend the whole output budget before answering; say so, or the agent
      // reads an empty answer as the model having nothing to say.
      const thought = typeof message.thinking === "string" && message.thinking.trim().length > 0;
      const budget = record(record(payload.request_options)?.options)?.num_predict ?? record(response)?.eval_count;
      parts.push(thought
        ? `(empty answer: the model spent its output${typeof budget === "number" ? ` (${budget} tokens)` : ""} thinking; pass think:false or raise options.num_predict)`
        : "(empty response)");
    }
    return header + parts.join("\n");
  }
  if (typeof response.response === "string") return header + response.response.trim();
  const answers = record(response.answers);
  if (answers) return header + decisionAnswerText(answers);
  const omitted = record(response.embeddings_omitted);
  if (Array.isArray(response.embeddings) || omitted) {
    const count = Array.isArray(response.embeddings) ? response.embeddings.length : omitted?.count;
    const where = omitted ? "vectors withheld; pass returnEmbeddings:true" : "vectors are in structuredContent";
    return `${header}embedding complete (${String(count ?? "?")} vectors); ${where}.`;
  }
  return undefined;
}

/** Below this confidence (choice, score), or with p(true) between 0.3 and 0.7, an answer is a coin toss. */
const UNSURE_CONFIDENCE = 0.25;
const UNSURE = " [unsure: treat as unknown]";

/** One line per decision answer: `kind: bug (p 0.97, confidence 0.94)`, `refund: true (p 0.99)`, `urgency: 1.20 of 0-2, likeliest "Soon" (confidence 0.40)`. */
export function decisionAnswerText(answers: Record<string, unknown>): string {
  const fixed = (value: unknown) => (typeof value === "number" ? value.toFixed(2) : "?");
  return Object.entries(answers).map(([name, raw]) => {
    const answer = record(raw) ?? {};
    const confidence = typeof answer.confidence === "number" ? `, confidence ${fixed(answer.confidence)}` : "";
    // A flat answer is the model saying it cannot tell: mark it, so nobody acts on its label.
    const unsure = typeof answer.confidence === "number" && answer.confidence < UNSURE_CONFIDENCE ? UNSURE : "";
    if (answer.type === "choice") {
      const choice = String(answer.choice ?? "?");
      return `${name}: ${choice} (p ${fixed(record(answer.probabilities)?.[choice])}${confidence})${unsure}`;
    }
    if (answer.type === "noul" && typeof answer.noul === "number") {
      const flat = answer.noul > 0.3 && answer.noul < 0.7 ? UNSURE : "";
      return `${name}: ${answer.noul >= 0.5 ? "true" : "false"} (p(true) ${fixed(answer.noul)})${flat}`;
    }
    if (answer.type === "score") {
      const legend = record(answer.legend) ?? {};
      const probabilities = Object.entries(record(answer.probabilities) ?? {})
        .filter((entry): entry is [string, number] => typeof entry[1] === "number");
      const likeliest = probabilities.sort((left, right) => right[1] - left[1])[0]?.[0];
      const top = Math.max(0, Object.keys(legend).length - 1);
      const label = likeliest === undefined ? "" : `, likeliest ${JSON.stringify(legend[likeliest] ?? likeliest)}`;
      return `${name}: ${fixed(answer.score)} of 0-${top}${label} (${confidence.slice(2) || "no confidence"})${unsure}`;
    }
    return `${name}: ${JSON.stringify(raw)}`;
  }).join("\n");
}

/** A cold load costs at least this much before the answer line mentions it. */
const NOTABLE_LOAD_MS = 250;

/** `loaded 8.9 s, evicted a:latest` when the call loaded a model or unloaded others to fit. */
function lifecycleClause(payload: Record<string, unknown>): string {
  const loadNs = record(payload.metrics)?.load_duration_ns;
  const loadMs = typeof loadNs === "number" ? loadNs / 1e6 : 0;
  const evicted = modelNames(record(record(payload.execution)?.memory_reservation)?.evicted_idle_models);
  return [
    loadMs > NOTABLE_LOAD_MS ? `loaded ${seconds(loadMs)}` : "",
    evicted.length > 0 ? `evicted ${evicted.join(", ")}` : "",
  ].filter(Boolean).join(", ");
}

/**
 * A model that looped until its output limit (core `output_check.repetition_loop`) is shown as one
 * copy plus a note; the untrimmed text stays in structuredContent.response for audit.
 */
function answerContent(content: string, payload: Record<string, unknown>): string {
  const check = record(payload.output_check);
  const keep = check?.unique_prefix_chars;
  if (check?.repetition_loop !== true || typeof keep !== "number") return content.trim();
  const characters = Array.from(content);
  const trimmed = characters.slice(0, keep).join("").trim();
  return `${trimmed}\n[freellama: ${characters.length - keep} repeated characters omitted here (full text in structuredContent); the model looped until its output limit — treat as low confidence]`;
}

const oneLine = (value: unknown, limit = 300) => {
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > limit ? `${text.slice(0, limit)}…` : text;
};

/** `closest of 3 items to the query:` plus its ranked lines, from the core's `nearest` ranking. */
function rankingText(ranked: Record<string, unknown>): string | undefined {
  if (ranked.mode === "query" && Array.isArray(ranked.matches)) {
    return `closest to the query:\n${nearestText(ranked as unknown as NearestResult)}`;
  }
  if (ranked.mode === "pairs" && Array.isArray(ranked.pairs)) {
    return `most similar pairs:\n${nearestText(ranked as unknown as NearestResult)}`;
  }
  return undefined;
}

/**
 * A job in one line: id, status, task, model, and why it is not finished yet. `task_jobs`
 * list prints one per job.
 */
export function jobLine(job: Record<string, unknown>): string {
  const model = job.selected_model ?? job.requested_model;
  const status = String(job.status ?? "?");
  const waiting = ["completed", "failed", "cancelled", "expired"].includes(status) || !job.reason ? "" : ` (${String(job.reason)})`;
  return [`${String(job.id ?? "?")}`, String(job.task ?? "?"), typeof model === "string" && model ? model : "-", `${status}${waiting}`].join("  ");
}

/**
 * The text of a `task_jobs` result: the job's status first, then what the agent acts on: the
 * answer, the decision answers, the nearest ranking, or the error. The whole receipt, with its
 * execution diagnostics, stays in structuredContent.
 */
export function jobsText(payload: Record<string, unknown>): string | undefined {
  if (Array.isArray(payload.jobs)) {
    return payload.jobs.length === 0 ? "no jobs" : payload.jobs.map((job) => jobLine(record(job) ?? {})).join("\n");
  }
  const job = record(payload.job);
  if (!job) return undefined;
  const lines = [jobLine(job)];
  if (typeof job.detail === "string" && job.detail) lines.push(job.detail);
  const result = record(job.result);
  const error = record(job.error);
  if (result) {
    const ranked = record(result.nearest);
    const ranking = ranked ? rankingText(ranked) : undefined;
    const answer = taskAnswerText(result);
    if (ranking) lines.push(ranking);
    else if (answer) lines.push(answer);
    else lines.push(oneLine(result.response ?? result, 1_500));
  } else if (error) {
    lines.push(`error: ${oneLine(error.error ?? error, 600)}${typeof error.code === "string" ? ` [${error.code}]` : ""}`);
  }
  return lines.join("\n");
}

/**
 * One line for a finished `ollama_manage` warm or unload: which model, whether it is resident,
 * its keep-alive, the load time and what it waited for. A deferred warm names its job instead.
 */
export function lifecycleText(action: string, model: string, payload: Record<string, unknown>): string | undefined {
  const job = record(payload.job);
  if (payload.deferred === true && job) return `${action} ${model} deferred: ${jobLine(job)}`;
  if (action === "unload") {
    const state = payload.unloaded === true ? "unloaded" : String(payload.status ?? "unload requested");
    return [`${String(payload.model ?? model)}: ${state}`, typeof payload.via === "string" ? `via ${payload.via}` : "",
      typeof payload.unload_ms === "number" ? `${payload.unload_ms} ms` : ""].filter(Boolean).join(", ");
  }
  if (action !== "warm") return undefined;
  const execution = record(payload.execution);
  const resident = record(payload.warm)?.loaded;
  const loadNs = record(payload.metrics)?.load_duration_ns;
  const waited = record(payload.admission)?.total_wait_ms;
  // The core reports its adaptive choice as an object; `value` is the TTL it sent to Ollama.
  const policy = record(execution?.keep_alive);
  const keepAlive = policy ? (policy.value !== undefined ? `${String(policy.value)}${typeof policy.mode === "string" ? ` (${policy.mode})` : ""}` : undefined) : execution?.keep_alive;
  return [
    `${String(record(payload.route)?.selected_model ?? model)}: ${resident === true ? "resident" : resident === false ? "not resident" : "warm requested"}`,
    keepAlive !== undefined && keepAlive !== null ? `keep_alive ${oneLine(keepAlive, 40)}` : "",
    typeof loadNs === "number" ? `load ${seconds(loadNs / 1e6)}` : "",
    typeof waited === "number" ? `waited ${waited} ms` : "",
  ].filter(Boolean).join(", ");
}

/** Batch counterpart of taskAnswerText: one labelled answer per item, in request order. */
export function batchAnswerText(payload: Record<string, unknown>): string | undefined {
  if (!Array.isArray(payload.results)) return undefined;
  const lines = payload.results.map((row) => {
    const item = record(row) ?? {};
    const id = String(item.id ?? "?");
    // One line per failure; the full error stays in structuredContent and the response file.
    if (item.ok !== true) {
      const error = record(item.error);
      const line = typeof error?.error === "string"
        ? refusalLine(JSON.stringify(error))
        : oneLine(JSON.stringify(item.error ?? item), 400);
      return `## ${id} (failed)\n${line}`;
    }
    const answer = taskAnswerText(record(item.response) ?? {}) ?? "(no answer)";
    return `## ${id}\n${answer}`;
  });
  return lines.join("\n\n");
}

// MCP clients that understand structuredContent receive the canonical object. The text block is
// the explicit `summary` when there is one, else the compact JSON capped at RESULT_TEXT_LIMIT so
// text-only clients still see the data. Task results pass their own text: the model's answer.
export function structuredResult(
  value: Record<string, unknown>,
  options: { text?: string } = {},
) {
  const text = options.text ?? resultSummary(value);
  return {
    content: [{ type: "text" as const, text }],
    structuredContent: value,
  };
}

// The Rust/native layer and Ollama both return JSON text. Parse it once here so the object can be
// handed back as `structuredContent` instead of being re-parsed by every consumer. A payload that
// doesn't parse is a genuine failure, not something to pass through as opaque text — and error
// results are exempt from output-schema validation, so reporting it that way is also the only
// shape that can't turn into a protocol-level McpError.
export function parsedResult(raw: string) {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch (error) {
    return errorResult(
      new Error(
        `upstream returned a payload that is not valid JSON: ${
          error instanceof Error ? error.message : String(error)
        }\n${raw}`,
      ),
    );
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return errorResult(new Error(`expected a JSON object from upstream, got: ${raw}`));
  }
  return structuredResult(value as Record<string, unknown>);
}

/** Error receipts also have structured content; only successful payloads can be transformed. */
export function isStructuredSuccess(
  result: ReturnType<typeof parsedResult>,
): result is ReturnType<typeof structuredResult> {
  return !("isError" in result && result.isError) && "structuredContent" in result
    && result.structuredContent !== undefined;
}

const escapeRegExp = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
/** Messages naming the managed API path or the configured serve host:port (loopback aliases included). */
const SERVE_ADDRESS_PATTERN = (() => {
  let url: URL;
  try {
    url = new URL(DEFAULT_SERVE_ENDPOINT);
  } catch {
    return /_freellama/i;
  }
  const { hostname, port } = url;
  const hosts = ["127.0.0.1", "localhost"].includes(hostname) ? ["127.0.0.1", "localhost"] : [hostname];
  const addresses = hosts.map((host) => escapeRegExp(port ? `${host}:${port}` : host));
  return new RegExp(`(?:_freellama|${addresses.join("|")})`, "i");
})();

/**
 * One line from a FreeLlama refusal body: the receipt behind a memory refusal is several KB, and
 * says no more to an agent than why it waited and what would let it run. The full receipt stays
 * in `structuredContent`.
 */
export function refusalLine(body: string, status?: number): string {
  let parsed: { error?: unknown; code?: unknown; resource_admission?: { assessment?: Record<string, unknown> } };
  try {
    parsed = (record(JSON.parse(body)) ?? {}) as typeof parsed;
  } catch {
    return body.trim().slice(0, 400) || `HTTP ${status ?? "error"}`;
  }
  const nestedMessage = record(parsed.error)?.message;
  const boundedMessage = typeof nestedMessage === "string" ? nestedMessage.slice(0, 400) : "";
  const error = typeof parsed.error === "string" ? parsed.error
    : boundedMessage.trim() ? boundedMessage : `HTTP ${status ?? "error"}`;
  const code = typeof parsed.code === "string" ? ` [${parsed.code}]` : "";
  const assessment = parsed.resource_admission?.assessment;
  // The core's managed-task refusals already state "needs X GB incl. Y GB reserve, Z GB free"
  // (or "free memory unknown"); add figures only to refusals that lack them (raw audio routes).
  if (!assessment || /\bneeds [\d.]+ GB incl\./.test(error)) return `${error}${code}`;
  const gb = (value: unknown) => (typeof value === "number" ? `${(value / 1e9).toFixed(1)} GB` : "?");
  const held = Array.isArray(assessment.held_by) && assessment.held_by.length > 0 ? ` (held by ${assessment.held_by.join(", ")})` : "";
  return `${error}${code}${held}: ${gb(assessment.effective_available_bytes)} available, ${gb(assessment.required_with_reserve_bytes)} needed with reserve; ` +
    "retry once the host frees memory (doctor{view:\"status\"} shows what holds it)";
}

export function errorResult(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  let receipt: Record<string, unknown> | undefined;
  try {
    const parsed: unknown = JSON.parse(message);
    if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)
      && typeof (parsed as Record<string, unknown>).error === "string") {
      receipt = parsed as Record<string, unknown>;
    }
  } catch {
    // Transport and extractor errors can be plain text; retain their diagnostic below.
  }
  // Native managed calls reach the local `freellama serve` process. A bare reqwest connection
  // error makes an agent retry a tool that cannot work yet; keep the original diagnostic, but
  // add the one safe recovery action. Do not do this for Ollama's direct endpoint errors: this
  // MCP package does not own that process or its configuration.
  const managedServeUnavailable =
    /(?:connection refused|connect error|failed to connect|error sending request)/i.test(message) &&
    SERVE_ADDRESS_PATTERN.test(message);
  // serve reached Ollama and Ollama was down: retrying the tool cannot help until a human starts it.
  const ollamaUnavailable =
    !managedServeUnavailable &&
    /(?:connection refused|connect error|failed to connect|error sending request)/i.test(message) &&
    /\/api\/(?:tags|ps|chat|generate|embed|show|version)/.test(message);
  const actionableMessage = managedServeUnavailable
    ? `${message}\n\nFreeLlama managed serve is unreachable. Start it with \`freellama serve\`, or set \`FREELLAMA_SERVE_ENDPOINT\` to a running managed endpoint; then retry. Run \`doctor\` to inspect the configured endpoint.`
    : ollamaUnavailable
      ? ollamaInstalled()
        ? `${message}\n\nOllama is not running at that address. Ask the user to start Ollama (the app, or \`ollama serve\`); retrying before that cannot succeed.`
        : `${message}\n\n${OLLAMA_MISSING} Ask the user to install it; nothing can run until then.`
      : receipt && message.length > 600 ? refusalLine(message) : message;
  return { content: [{ type: "text" as const, text: actionableMessage }], isError: true,
    ...(receipt ? { structuredContent: receipt } : {}) };
}

/**
 * Strip the raw embedding matrix out of a `run_task` result, leaving enough to verify the call
 * worked (how many vectors, what dimensionality, the leading values of the first one).
 *
 * `run_task` advertises that the orchestrator "pays only the JSON wrapper (~hundreds of tokens,
 * regardless of output size)". For embeddings that was not true: measured against a live server,
 * one 768-dim `nomic-embed-text` vector came back as 17,471 bytes of pretty-printed JSON — about
 * 4,400 tokens, into the context of the model that was supposed to be *offloading* work. A batch
 * of twenty inputs would have been ~90k. Vectors are also the one kind of output a language model
 * can do nothing useful with by reading it.
 *
 * Returns `null` when there is no embedding matrix to strip, so non-embedding tasks pass through
 * untouched.
 */
export function summarizeEmbeddings(
  payload: Record<string, unknown>,
  options: { saved?: boolean } = {},
): Record<string, unknown> | null {
  const response = payload.response;
  if (response === null || typeof response !== "object") return null;
  const { embeddings, ...rest } = response as Record<string, unknown>;
  if (!Array.isArray(embeddings) || embeddings.length === 0) return null;
  const first = embeddings[0];
  const dimensions = Array.isArray(first) ? first.length : null;
  return {
    ...payload,
    response: {
      ...rest,
      embeddings_omitted: {
        count: embeddings.length,
        dimensions,
        preview: Array.isArray(first) ? first.slice(0, 8) : null,
        note:
          "Vectors withheld to keep them out of the orchestrator's context — " +
          (options.saved ? "the full matrix is in response_file (read_response section \"embeddings\"), or pass " : "pass ") +
          "`returnEmbeddings: true` to get the full matrix (a single 768-dim vector is ~4,400 " +
          "tokens). `preview` is the first 8 values of the first vector, enough to confirm the " +
          "call really produced numbers.",
      },
    },
  };
}
