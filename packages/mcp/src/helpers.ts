// Shared MCP result helpers, Zod parameter schemas, the confidence gate, and payload trimming.
import { existsSync, realpathSync, statSync } from "node:fs";
import path from "node:path";
import { z } from "zod";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS, DEFAULT_PULL_TIMEOUT_SECONDS } from "./config.js";

/**
 * Find the first existing file explicitly named in a shell command, confined to the workspace.
 *
 * Bash-agent calls carry only a command string, unlike Octocode calls with a structured path.
 * This deliberately does not guess from globs, `find -name`, or answer prose: a citation path is
 * emitted only when the command itself contains an existing file path inside the allowed root.
 */
export function extractExistingWorkspacePath(command: string, workspace: string): string | null {
  let root: string;
  try {
    root = realpathSync(workspace);
  } catch {
    return null;
  }
  const tokens = command.match(/(?:[^\s"'\\]+|"(?:\\.|[^"])*"|'[^']*')+/g) ?? [];
  const patternFlags = new Set(["-name", "-iname", "-path", "-ipath", "-regex", "-iregex", "-wholename"]);
  for (let index = 0; index < tokens.length; index += 1) {
    const raw = tokens[index] ?? "";
    if (index > 0 && patternFlags.has(tokens[index - 1] ?? "")) continue;
    const token = raw
      .replace(/^["']|["']$/g, "")
      .replace(/[|;&)]+$/g, "")
      .replace(/:\d+(?::\d+)?$/, "");
    if (!token || token.startsWith("-") || /[*?$`]/.test(token)) continue;
    const candidate = path.isAbsolute(token) ? token : path.resolve(root, token);
    if (!existsSync(candidate)) continue;
    try {
      const resolved = realpathSync(candidate);
      const relative = path.relative(root, resolved);
      const withinRoot = relative && relative !== ".." && !relative.startsWith(`..${path.sep}`);
      if (withinRoot && statSync(resolved).isFile()) return relative;
    } catch {
      // A raced-away or unreadable token is not citation evidence; inspect the next token.
    }
  }
  return null;
}

export async function ollamaFetch(
  endpoint: string | undefined,
  path: string,
  init: { method?: string; body?: unknown; timeoutMs?: number; parse?: "json" | "ndjson" } = {},
): Promise<unknown> {
  const base = (endpoint ?? DEFAULT_OLLAMA_ENDPOINT).replace(/\/$/, "");
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(),
    init.timeoutMs ?? DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000,
  );
  try {
    const response = await fetch(`${base}${path}`, {
      method: init.method ?? "GET",
      headers: init.body ? { "content-type": "application/json" } : undefined,
      body: init.body ? JSON.stringify(init.body) : undefined,
      signal: controller.signal,
    });
    const text = await response.text();
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

/**
 * Collapse Ollama `/api/pull` NDJSON into one object an agent can read.
 *
 * `stream: false` hid every byte of a multi-GB download behind a blocking wait. Each NDJSON
 * line is a status snapshot (`pulling manifest`, layer digest, `completed`/`total`). Keep the
 * last line and the last line that had a byte total so the tool result reports percent done
 * (or `success` for an already-installed tag) instead of a silent hang.
 */
export function summarizeOllamaPullStream(text: string): Record<string, unknown> {
  const events: Record<string, unknown>[] = [];
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        events.push(parsed as Record<string, unknown>);
      }
    } catch {
      // Non-JSON noise on the stream is ignored; the last valid event is the result.
    }
  }
  if (events.length === 0) {
    if (!text.trim()) return { status: "empty" };
    try {
      const parsed = JSON.parse(text) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return parsed as Record<string, unknown>;
      }
    } catch {
      return { status: "unparsed", raw: clipText(text, 500) };
    }
    return { status: "unparsed", raw: clipText(text, 500) };
  }
  const last = events[events.length - 1] ?? {};
  const withBytes = [...events]
    .reverse()
    .find((event) => typeof event.total === "number" && (event.total as number) > 0);
  const completed = typeof withBytes?.completed === "number" ? (withBytes.completed as number) : null;
  const total = typeof withBytes?.total === "number" ? (withBytes.total as number) : null;
  const percent =
    completed != null && total != null && total > 0
      ? Math.round((completed / total) * 1000) / 10
      : null;
  // Ollama reports failures as an {"error": ...} event on an HTTP 200 stream (verified live: a
  // bogus tag yields `pulling manifest` then `{"error":"pull model manifest: ..."}`), so nothing
  // at the transport layer fails. Without this, a failed pull came back success-shaped.
  const status = typeof last.error === "string" && last.error ? "error" : last.status;
  return {
    ...last,
    ...(status === undefined ? {} : { status }),
    progress: {
      events: events.length,
      completed,
      total,
      percent,
      lastStatus: last.status ?? null,
    },
  };
}

export async function ollamaPull(
  endpoint: string | undefined,
  model: string,
  timeoutSeconds?: number,
): Promise<Record<string, unknown>> {
  const raw = await ollamaFetch(endpoint, "/api/pull", {
    method: "POST",
    body: { name: model, stream: true },
    timeoutMs: (timeoutSeconds ?? DEFAULT_PULL_TIMEOUT_SECONDS) * 1000,
    parse: "ndjson",
  });
  if (typeof raw === "string") return summarizeOllamaPullStream(raw);
  if (raw && typeof raw === "object" && !Array.isArray(raw)) return raw as Record<string, unknown>;
  return { status: "empty" };
}

// Self-evident params carry no `.describe()`: the name says it, the default is in the server
// instructions, and every description is re-sent on every request. Only params whose BEHAVIOUR
// isn't obvious from the name keep one.
export const endpointParam = z.string().min(1).optional().describe("serve endpoint, default :11435");
export const ollamaEndpointParam = z.string().min(1).optional().describe("Ollama endpoint, default :11434");
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
export const MCP_TASK_KINDS = [...TASK_KINDS, "code_review"] as const;
export type McpTaskKind = (typeof MCP_TASK_KINDS)[number];
export const taskParam = z.enum(MCP_TASK_KINDS).describe("code_review=coding.");

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
  .optional()
  .describe('"balanced"/"quality" need a policy; "fastest" does not');
export const executionPreferenceParam = z
  .enum(["auto", "prefer_cpu", "prefer_gpu"])
  .optional()
  .describe(
    'Backend hint. "auto" uses guarded runtime feedback; prefer_* falls back when no eligible operator-assigned model exists.',
  );
export const minPlacementEvidenceParam = z
  .enum(["configured", "observed"])
  .optional()
  .describe('"observed" fails closed unless Ollama /api/ps confirms the selected processor');
// Router grades only "low" | "medium". A low/capability-only pick once selected a far-too-small
// model for a demanding task — the answer still looked confident.
const CONFIDENCE_RANK: Record<string, number> = { low: 1, medium: 2 };

export const minConfidenceParam = z
  .enum(["low", "medium"])
  .optional()
  .describe('Fail closed below this. "medium" needs policy AND benchmark; "low" accepts capability metadata.');

/**
 * Fail closed when a route decision isn't backed well enough for what the caller asked.
 *
 * Returns an error result rather than throwing, so the refusal reaches the caller as a normal
 * tool result carrying the rejected decision — the point is to hand back enough to decide what to
 * do next, not merely to say no.
 */
export function belowConfidence(decision: Record<string, unknown>, minConfidence?: "low" | "medium") {
  if (!minConfidence) return null;
  const actual = typeof decision.confidence === "string" ? decision.confidence : "low";
  // Unknown grades rank 0, matching the core gate (`unwrap_or(0)`). Ranking them as "low"
  // would fail-open against `minConfidence: "low"`.
  if ((CONFIDENCE_RANK[actual] ?? 0) >= (CONFIDENCE_RANK[minConfidence] ?? 0)) return null;
  return errorResult(
    new Error(
      `Route refused: confidence is "${actual}" (evidence: ${decision.evidence}), below the ` +
        `requested minimum "${minConfidence}". Selected model would have been ` +
        `"${decision.selected_model}" for reasons [${(decision.reasons as string[] | undefined)?.join(", ")}].\n` +
        "This is a fail-closed refusal, not a failure: the local router cannot justify this pick. " +
        "Escalate to your own model, pass an explicit `model`, or run `bench-all` and configure a " +
        "task policy to raise the evidence level.",
    ),
  );
}

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
  .optional()
  .describe('Additional hard requirements, e.g. ["vision"] or ["tools"]. Fails closed if unmet.');

// The stable parts are typed so an MCP client can construct a batch without guessing. The
// forwarded Ollama controls remain deliberately open because Ollama evolves them independently.
export const batchTaskParam = z.object({
  task: taskParam,
  objective: objectiveParam,
  model: z.string().min(1).optional(),
  sessionId: z.string().uuid().optional(),
  contextTokens: z.number().int().positive().optional(),
  executionPreference: executionPreferenceParam,
  minPlacementEvidence: minPlacementEvidenceParam,
  requiredCapabilities: requiredCapabilitiesParam,
  priority: z.enum(["interactive", "normal", "background"]).optional(),
  prompt: z.string().min(1).optional(),
  messages: z.array(z.object({ role: z.enum(["system", "user", "assistant", "tool"]), content: z.string() }).passthrough()).min(1).optional(),
  images: z.array(z.string().min(1)).min(1).optional(),
  input: z.union([z.string().min(1), z.array(z.string().min(1)).min(1)]).optional(),
  tools: z.array(z.record(z.unknown())).min(1).optional(),
  keepAlive: z.string().min(1).optional(),
  format: z.union([z.literal("json"), z.record(z.unknown())]).optional(),
  think: z.union([z.boolean(), z.enum(["low", "medium", "high"])]).optional(),
  options: z.record(z.unknown()).optional(),
  logprobs: z.boolean().optional(),
  topLogprobs: z.number().int().nonnegative().optional(),
  minConfidence: minConfidenceParam,
}).strict();

export const batchItemParam = z.object({
  id: z.string().min(1),
  independent: z.literal(true),
  task: batchTaskParam,
}).strict();

// Output schemas describe FreeLlama-owned decision envelopes while leaving Ollama-owned payloads
// open. This gives a client useful validation without making a newly-added upstream response field
// into a protocol failure.
export const objectResultSchema = z.object({}).passthrough();
export const doctorResultSchema = z.object({
  summary: z.string().optional(), endpoint: z.string().optional(), scheduler: z.unknown().optional(),
}).passthrough();
export const sessionResultSchema = z.object({
  session_id: z.string().optional(), deleted: z.boolean().optional(),
}).passthrough();
export const manageResultSchema = z.object({
  status: z.string().optional(), progress: z.unknown().optional(),
}).passthrough();
export const deleteResultSchema = z.object({
  deleted: z.string(),
}).passthrough();
export const modelsResultSchema = z.object({
  models: z.array(z.record(z.unknown())).optional(), tags: z.array(z.record(z.unknown())).optional(), page: z.unknown().optional(),
}).passthrough();
export const taskResultSchema = z.object({
  selected_model: z.string().optional(), context_window_fit: z.string().optional(), execution: z.unknown().optional(), response: z.unknown().optional(), telemetry: z.unknown().optional(),
}).passthrough();
export const batchResultSchema = z.object({
  results: z.unknown().optional(), telemetry: z.unknown().optional(),
}).passthrough();
export const researchResultSchema = z.object({
  answer: z.string(), summary: z.string(), citations: z.unknown().optional(), verification: z.unknown(), usage: z.unknown().optional(), telemetry: z.unknown().optional(),
}).passthrough();

export type ExternalCost = { model: string; input: number; output: number };

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

// Pretty-printing is easier for a model to read but is pure overhead once a payload is large:
// a single 768-dim embedding measured 10,293 bytes compact vs 17,471 pretty — ~1,800 wasted
// tokens of indentation for zero information. Stay pretty while it's cheap, go compact when it
// isn't.
/**
 * Clip text to `limit` characters, keeping BOTH ends and saying how much was dropped.
 *
 * Four call sites each rolled their own `slice()`, three of them with no marker — so a reader could
 * not tell a complete value from a cut one. That is how a `delegate_research` evidence line came
 * back as `... --exclude-dir={node_modules,target,.venv,__pycach`: a hard 120-char head slice
 * through the middle of a command, which is unauditable precisely when auditing matters.
 *
 * Head-biased because the start of a command or message identifies it, with a tail so the target
 * of a long `grep` survives. Mirrors `agent_context.clip` on the adapter side, deliberately: the
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

const PRETTY_PRINT_MAX_BYTES = 8 * 1024;

export function serialize(value: unknown): string {
  const compact = JSON.stringify(value);
  return compact.length > PRETTY_PRINT_MAX_BYTES ? compact : JSON.stringify(value, null, 2);
}

/** A short human/backward-client cue; canonical result data stays in structuredContent. */
function resultSummary(value: Record<string, unknown>): string {
  const explicit = value.summary;
  if (typeof explicit === "string" && explicit.trim()) return clipText(explicit.trim(), 500);
  const keys = Object.keys(value);
  return `Structured result available (${keys.slice(0, 8).join(", ") || "empty object"}).`;
}

// MCP clients that understand structuredContent receive the canonical object. Repeating a large
// JSON serialization in TextContent wastes agent context (especially doctor and raw model views),
// so the text block is a compact compatibility cue rather than a second transport encoding.
export function structuredResult(value: Record<string, unknown>, options: { legacyJson?: boolean } = {}) {
  return {
    content: [{ type: "text" as const, text: options.legacyJson ? serialize(value) : resultSummary(value) }],
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

export function errorResult(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  // Native managed calls reach the local `freellama serve` process. A bare reqwest connection
  // error makes an agent retry a tool that cannot work yet; keep the original diagnostic, but
  // add the one safe recovery action. Do not do this for Ollama's direct endpoint errors: this
  // MCP package does not own that process or its configuration.
  const managedServeUnavailable =
    /(?:connection refused|connect error|failed to connect|error sending request)/i.test(message) &&
    /(?:_freellama|127\.0\.0\.1:11435|localhost:11435)/i.test(message);
  const actionableMessage = managedServeUnavailable
    ? `${message}\n\nFreeLlama managed serve is unreachable. Start it with \`freellama serve\`, or set \`FREELLAMA_SERVE_ENDPOINT\` to a running managed endpoint; then retry. Run \`doctor\` to inspect the configured endpoint.`
    : message;
  return { content: [{ type: "text" as const, text: actionableMessage }], isError: true };
}

const adapterCallSchema = z
  .object({
    raw_name: z.string().optional(),
    status: z.string().optional(),
    arguments: z
      .object({
        tool: z.string().optional(),
        command: z.string().optional(),
        queries: z.object({ path: z.string().optional() }).passthrough().optional(),
      })
      .passthrough()
      .optional(),
  })
  .passthrough();

const adapterResultSchema = z.object({
  final_answer: z.string(),
  tool_calls: z.array(adapterCallSchema).default([]),
  usage: z
    .object({
      input_tokens: z.number().nullable().optional(),
      output_tokens: z.number().nullable().optional(),
    })
    .passthrough()
    .default({}),
  model_metadata: z
    .object({
      context_compactions: z.number().int().nonnegative().optional(),
      runtime_config: z.record(z.string(), z.unknown()).optional(),
      context_management: z
        .object({
          token_counting: z.enum(["configured_estimate", "model_calibrated_estimate"]),
          estimate_scale: z.number().positive(),
          calibration_samples: z.number().int().nonnegative(),
          pinned_overflow: z.enum(["error", "clip"]),
          compactions: z.number().int().nonnegative(),
        })
        .passthrough()
        .optional(),
    })
    .passthrough()
    .optional(),
});

export type AdapterResult = z.infer<typeof adapterResultSchema>;

/** Parse adapter result.json. Valid JSON missing `final_answer`/`tool_calls` used to throw later. */
export function parseAdapterResult(raw: string): AdapterResult {
  return adapterResultSchema.parse(JSON.parse(raw));
}

// Every MCP tool declares a permissive object output boundary. That makes structuredContent
// machine-declared without falsely freezing upstream Ollama fields: a strict schema previously
// turned an undocumented /api/show `requires` field into a hard McpError. Compact, owned results
// (for example doctor summary) can gain strict field schemas incrementally; proxy-shaped results
// must remain forward-compatible.

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
