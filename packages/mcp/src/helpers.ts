// Shared MCP result helpers, Zod parameter schemas, the confidence gate, and payload trimming.
import { z } from "zod";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS } from "./config.js";

export async function ollamaFetch(
  endpoint: string | undefined,
  path: string,
  init: { method?: string; body?: unknown; timeoutMs?: number } = {},
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
    return text ? JSON.parse(text) : {};
  } finally {
    clearTimeout(timeout);
  }
}

// Self-evident params carry no `.describe()`: the name says it, the default is in the server
// instructions, and every description is re-sent on every request. Only params whose BEHAVIOUR
// isn't obvious from the name keep one.
export const endpointParam = z.string().optional().describe("serve endpoint, default :11435");
export const ollamaEndpointParam = z.string().optional().describe("Ollama endpoint, default :11434");
export const taskParam = z.string().describe('e.g. completion, code_repair, vision, embedding');
export const objectiveParam = z
  .enum(["fastest", "balanced", "quality"])
  .optional()
  .describe('"balanced"/"quality" need a configured policy; "fastest" does not');
// The server grades every route decision (`route_evidence` in packages/rust-core/src/platform/routing.rs): "medium" only
// when the task has BOTH a configured policy and benchmark data, "low" otherwise — there is no
// "high". A "low"/capability_metadata_only decision is exactly what returned `qwen2.5:0.5b` for
// code repair on this machine. Unchecked, that answer comes back looking like any other.
const CONFIDENCE_RANK: Record<string, number> = { low: 1, medium: 2 };

export const minConfidenceParam = z
  .enum(["low", "medium"])
  .optional()
  .describe('Fail closed below this. "medium" needs a policy AND benchmark data; "low" (default) accepts capability metadata alone. The refusal names what was missing');

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
  if ((CONFIDENCE_RANK[actual] ?? 1) >= (CONFIDENCE_RANK[minConfidence] ?? 1)) return null;
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

export const requiredCapabilitiesParam = z
  .array(z.string())
  .optional()
  .describe('e.g. ["vision"], ["tools"]. Fails closed rather than picking a model that can\'t do it');

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
