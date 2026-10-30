// Shared MCP result helpers, Zod parameter schemas, the confidence gate, and payload trimming.
import * as z from "zod";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS, DEFAULT_SERVE_ENDPOINT } from "./config.js";

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
] as const;
const MCP_TASK_KINDS = [...TASK_KINDS, "code_review"] as const;
type McpTaskKind = (typeof MCP_TASK_KINDS)[number];
export const taskParam = z
  .enum(MCP_TASK_KINDS)
  .default("completion")
  .describe("caller owns prompts and output format.");

/**
 * MCP accepts the name humans and agents naturally use for a review, while the Rust routing
 * contract deliberately keeps one coding profile. Normalize at the boundary so every downstream
 * policy, receipt, and benchmark continues to use the canonical task kind.
 */
export function canonicalTaskKind(task: McpTaskKind): (typeof TASK_KINDS)[number] {
  return task === "code_review" ? "coding" : task;
}
export const objectiveParam = z
  .enum(["fastest", "balanced", "quality"])
  .optional();
export const executionPreferenceParam = z
  .enum(["auto", "prefer_cpu", "prefer_gpu"])
  .optional();
export const minPlacementEvidenceParam = z
  .enum(["configured", "observed"])
  .optional();
// Router grades only "low" | "medium"; the Rust core enforces the floor (`enforce_min_confidence`).
export const minConfidenceParam = z
  .enum(["low", "medium"])
  .optional();

export const REQUIRED_CAPABILITIES = [
  "completion",
  "tools",
  "vision",
  "audio",
  "thinking",
  "embedding",
] as const;
export const requiredCapabilitiesParam = z
  .array(z.enum(REQUIRED_CAPABILITIES))
  .max(REQUIRED_CAPABILITIES.length)
  .optional();

export const systemPromptParam = z.string().optional()
  .describe("Local model instructions; verbatim.");
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

// The stable parts are typed so an MCP client can construct a batch without guessing. The
// forwarded Ollama controls remain deliberately open because Ollama evolves them independently.
const batchTaskParam = z.object({
  // Nested `task` shares the top-level description; repeating it re-sends the text every request.
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
  stopOnRepetition: z.boolean().optional().describe("Stream and stop once output repeats itself; keeps all generated text"),
  prompt: z.string().min(1).optional(),
  systemPrompt: systemPromptParam,
  messages: messagesParam,
  images: z.array(z.string().min(1)).min(1).optional(),
  input: z.union([z.string().min(1), z.array(z.string().min(1)).min(1)]).optional(),
  tools: localToolsParam,
  keepAlive: z.string().min(1).optional(),
  format: z.union([z.literal("json"), z.record(z.string(), z.unknown())]).optional(),
  think: z.union([z.boolean(), z.enum(["low", "medium", "high"])]).optional(),
  options: z.record(z.string(), z.unknown()).optional(),
  logprobs: z.boolean().optional(),
  topLogprobs: z.number().int().nonnegative().optional(),
  minConfidence: minConfidenceParam,
}).strict();

/** Field names a `tasks[].task` item accepts; derived from the schema so hints cannot drift. */
export const BATCH_TASK_FIELDS: ReadonlySet<string> = new Set(Object.keys(batchTaskParam.shape));

export const batchItemParam = z.object({
  id: z.string().min(1),
  independent: z.literal(true),
  task: batchTaskParam,
}).strict();

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

/**
 * Clip text to `limit` characters, keeping BOTH ends and saying how much was dropped.
 *
 * Four call sites each rolled their own `slice()`, three of them with no marker — so a reader could
 * not tell a complete value from a cut one. That is how a `delegate_research` evidence line came
 * back as `... --exclude-dir={node_modules,target,.venv,__pycach`: a hard 120-char head slice
 * through the middle of a command, which is unauditable precisely when auditing matters.
 *
 * Head-biased because the start of a command or message identifies it, with a tail so the target
 * of a long `grep` survives. Mirrors `clip` in src/research/fit.ts on the adapter side, deliberately: the
 * same rule on both sides of the boundary.
 */
export function clipText(text: string, limit: number): string {
  if (text.length <= limit) return text;
  const marker = `… [${text.length - limit} more chars] …`;
  const usable = Math.max(limit - marker.length, 0);
  if (usable === 0) return text.slice(0, limit);
  const head = Math.floor((usable * 2) / 3);
  return text.slice(0, head) + marker + text.slice(-(usable - head));
}

/** A short human/backward-client cue; canonical result data stays in structuredContent. */
function resultSummary(value: Record<string, unknown>): string {
  const explicit = value.summary;
  if (typeof explicit === "string" && explicit.trim()) return clipText(explicit.trim(), 500);
  const keys = Object.keys(value);
  return `Structured result available (${keys.slice(0, 8).join(", ") || "empty object"}).`;
}

/** Text budget for a model answer in TextContent; the full answer stays in structuredContent. */
export const ANSWER_TEXT_MAX_CHARS = 8 * 1024;

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
  const header = typeof model === "string" && model ? `[${model}] ` : "";
  const message = record(response.message);
  if (message) {
    const parts: string[] = [];
    if (typeof message.content === "string" && message.content.trim()) parts.push(answerContent(message.content, payload));
    if (Array.isArray(message.tool_calls) && message.tool_calls.length > 0) {
      parts.push(`tool_calls: ${JSON.stringify(message.tool_calls)}`);
    }
    if (parts.length === 0) parts.push("(empty response)");
    return clipText(header + parts.join("\n"), ANSWER_TEXT_MAX_CHARS);
  }
  if (typeof response.response === "string") return clipText(header + response.response.trim(), ANSWER_TEXT_MAX_CHARS);
  const omitted = record(response.embeddings_omitted);
  if (Array.isArray(response.embeddings) || omitted) {
    const count = Array.isArray(response.embeddings) ? response.embeddings.length : omitted?.count;
    const where = omitted ? "vectors withheld; pass returnEmbeddings:true" : "vectors are in structuredContent";
    return `${header}embedding complete (${String(count ?? "?")} vectors); ${where}.`;
  }
  return undefined;
}

/**
 * A model that looped until the output cap (core `output_check.repetition_loop`) is shown as one
 * copy plus a note; the untrimmed text stays in structuredContent.response for audit.
 */
function answerContent(content: string, payload: Record<string, unknown>): string {
  const check = record(payload.output_check);
  const keep = check?.unique_prefix_chars;
  if (check?.repetition_loop !== true || typeof keep !== "number") return content.trim();
  const characters = Array.from(content);
  const trimmed = characters.slice(0, keep).join("").trim();
  return `${trimmed}\n[freellama: trimmed ${characters.length - keep} repeated characters; the model looped until num_predict — treat as low confidence]`;
}

/** Batch counterpart of taskAnswerText: one labelled answer per item, in request order. */
export function batchAnswerText(payload: Record<string, unknown>): string | undefined {
  if (!Array.isArray(payload.results)) return undefined;
  const perItem = Math.max(512, Math.floor(ANSWER_TEXT_MAX_CHARS / Math.max(payload.results.length, 1)));
  const lines = payload.results.map((row) => {
    const item = record(row) ?? {};
    const id = String(item.id ?? "?");
    if (item.ok !== true) return `## ${id} (failed)\n${clipText(JSON.stringify(item.error ?? item), perItem)}`;
    const answer = taskAnswerText(record(item.response) ?? {}) ?? "(no answer)";
    return `## ${id}\n${clipText(answer, perItem)}`;
  });
  return lines.join("\n\n");
}

// MCP clients that understand structuredContent receive the canonical object. Repeating a large
// JSON serialization in TextContent wastes agent context (especially doctor and raw model views),
// so the text block is a compact compatibility cue rather than a second transport encoding.
// Task results are the exception: their text is the model's answer, since that is the payload.
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
        }\n${clipText(raw, 2000)}`,
      ),
    );
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return errorResult(new Error(`expected a JSON object from upstream, got: ${clipText(raw, 500)}`));
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
    /\/api\/(?:tags|ps|chat|generate|embed|show)/.test(message);
  const actionableMessage = managedServeUnavailable
    ? `${message}\n\nFreeLlama managed serve is unreachable. Start it with \`freellama serve\`, or set \`FREELLAMA_SERVE_ENDPOINT\` to a running managed endpoint; then retry. Run \`doctor\` to inspect the configured endpoint.`
    : ollamaUnavailable
      ? `${message}\n\nOllama is not running at that address. Ask the user to start Ollama (the app, or \`ollama serve\`); retrying before that cannot succeed.`
      : message;
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
export function summarizeEmbeddings(payload: Record<string, unknown>): Record<string, unknown> | null {
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
          "Vectors withheld to keep them out of the orchestrator's context — pass " +
          "`returnEmbeddings: true` to get the full matrix (a single 768-dim vector is ~4,400 " +
          "tokens). `preview` is the first 8 values of the first vector, enough to confirm the " +
          "call really produced numbers.",
      },
    },
  };
}
