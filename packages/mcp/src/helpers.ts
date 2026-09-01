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
export const taskParam = z.enum(TASK_KINDS).describe("Exact managed task profile.");
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

// Results carry both `structuredContent` (the parsed object) and the serialized JSON as a text
// block, per the spec's backwards-compatibility SHOULD. No `outputSchema` is declared — see below.
export function structuredResult(value: Record<string, unknown>) {
  return {
    content: [{ type: "text" as const, text: serialize(value) }],
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
  return { content: [{ type: "text" as const, text: message }], isError: true };
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

// Output schemas were removed deliberately. They cost ~1,086 tokens on EVERY request to buy
// client-side JSON-Schema validation — and that validation was itself a hazard: a strict schema
// turned any undeclared upstream field into a hard `McpError` (caught live when /api/show returned
// an undocumented `requires` field). Verified against the SDK: `structuredContent` still reaches
// the client with no `outputSchema` declared, so callers keep the parsed object and lose only the
// validation. Reinstate per-tool if a consumer needs machine-checked output.

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
