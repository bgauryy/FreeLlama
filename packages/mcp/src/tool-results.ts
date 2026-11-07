import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { summarizeEmbeddings } from "./helpers.js";
import { responseStore, type ResponseStore } from "./response-store.js";

type ToolResult = { structuredContent?: unknown; isError?: boolean; _meta?: Record<string, unknown> };
type Fields = Record<string, unknown>;

const nsToMs = (value: unknown) => (typeof value === "number" ? Math.round(value / 1e6) : undefined);
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : undefined;

/**
 * Where the time went, from the managed receipt when the call had one: waiting for admission,
 * loading the model (a switch), reading the prompt, and generating. Ollama measures the last three.
 */
function phases(content: Fields) {
  const admission = object(content.admission);
  const metrics = object(content.metrics);
  const found = {
    queue_ms: typeof admission?.total_wait_ms === "number" ? admission.total_wait_ms : undefined,
    load_ms: nsToMs(metrics?.load_duration_ns),
    prompt_ms: nsToMs(metrics?.prompt_duration_ns),
    output_ms: nsToMs(metrics?.output_duration_ns),
  };
  const known = Object.entries(found).filter(([, value]) => value !== undefined);
  return known.length > 0 ? Object.fromEntries(known) : undefined;
}

/** Add `timing` to a tool result: wall time of the whole call, plus its phases when known. */
export function withTiming<T extends ToolResult>(result: T, elapsedMs: number, startedAt: string): T {
  const content = object(result.structuredContent);
  const base = { elapsed_ms: elapsedMs, started_at: startedAt };
  if (content) {
    const split = phases(content);
    return { ...result, structuredContent: { ...content, timing: split ? { ...base, phases: split } : base } };
  }
  // No structured result (most errors): the timing still travels in the result's metadata.
  return { ...result, _meta: { ...result._meta, "freellama/timing": base } };
}

/** `source` keeping only `keys` that are present. */
function pick(source: Fields | undefined, keys: string[]): Fields | undefined {
  if (!source) return undefined;
  const kept = Object.fromEntries(keys.filter((key) => source[key] !== undefined).map((key) => [key, source[key]]));
  return Object.keys(kept).length > 0 ? kept : undefined;
}

/** Keep the core's routing explanation and candidate totals; detailed phase estimates stay saved. */
function compactCompletionPlanning(value: unknown): Fields | undefined {
  const planning = object(value);
  if (!planning) return undefined;
  const compact = pick(planning, ["mode", "applied", "snapshot_only", "payload_fit_rejections"]) ?? {};
  if (Array.isArray(planning.candidates)) {
    compact.candidates = planning.candidates.map((candidate) =>
      pick(object(candidate), ["model", "backend", "status", "estimated_completion_ns"]) ?? {});
  }
  return compact;
}

/**
 * The part of one managed receipt an agent acts on: the answer whole, which model gave it, how it
 * ended, what it cost, why and where it ran, and what it unloaded. Detailed routing phases,
 * host-telemetry snapshots and feedback internals stay in the response file.
 */
function compactReceipt(receipt: Fields): Fields {
  const execution = object(receipt.execution);
  const reservation = pick(object(execution?.memory_reservation), ["evicted_idle_models", "eviction", "required_available_bytes", "source"]);
  const completionPlanning = compactCompletionPlanning(execution?.completion_planning);
  const compact: Fields = {};
  for (const key of ["selected_model", "response", "output_check", "metrics", "telemetry", "scope", "warm", "job", "deferred",
    "nearest", "files_read", "embedding_requests", "embedding_identity", "accepted_job_ids"]) {
    if (receipt[key] !== undefined) compact[key] = receipt[key];
  }
  const response = withoutThinking(object(receipt.response));
  if (response) compact.response = response;
  const route = pick(object(receipt.route), ["selected_model", "task", "confidence", "reasons", "output_reserve_tokens"]);
  if (route) compact.route = route;
  const admission = pick(object(receipt.admission), ["mode", "queue_wait_ms", "transition_wait_ms", "resource_wait_ms", "total_wait_ms"]);
  if (admission) compact.admission = admission;
  if (execution) {
    compact.execution = {
      ...pick(execution, ["placement", "backend", "reason", "observation", "lifecycle", "context_sizing", "keep_alive", "preference_satisfied"]),
      ...(completionPlanning ? { completion_planning: completionPlanning } : {}),
      ...(reservation ? { memory_reservation: reservation } : {}),
    };
  }
  return compact;
}

/**
 * The response with its reasoning text replaced by its length (`thinking_chars`), or `undefined`
 * when it carried none. The text stays in the response file (section `thinking`).
 */
function withoutThinking(response: Fields | undefined): Fields | undefined {
  if (!response) return undefined;
  const message = object(response.message);
  if (typeof message?.thinking === "string") {
    const { thinking, ...rest } = message;
    return { ...response, message: { ...rest, thinking_chars: thinking.length } };
  }
  if (typeof response.thinking === "string") {
    const { thinking, ...rest } = response;
    return { ...rest, thinking_chars: thinking.length };
  }
  return undefined;
}

/** `run_task` results: one receipt, or a batch of them. Everything else passes through whole. */
function compactTaskResult(content: Fields): Fields {
  if (Array.isArray(content.results)) {
    return {
      ...content,
      results: content.results.map((row) => {
        const item = object(row);
        const response = object(item?.response);
        return item && response ? { ...item, response: compactReceipt(response) } : row;
      }),
    };
  }
  return content.route !== undefined || content.execution !== undefined ? compactReceipt(content) : content;
}

/** Research fields an agent rarely reads inline: per-turn receipts, context accounting, cost notes,
 *  the step-by-step trail and every observation the model saw. */
const RESEARCH_FILE_SECTIONS = ["observations", "evidence", "execution", "contextManagement", "telemetry"];

/** `delegate_research`: the answer, its grading and citations inline; the trail in the file. */
function compactResearchResult(content: Fields): { shaped: Fields; sections: string[] } {
  const shaped = { ...content };
  const sections = RESEARCH_FILE_SECTIONS.filter((key) => content[key] !== undefined);
  for (const key of sections) delete shaped[key];
  return { shaped, sections };
}

/** Tools whose results are views of saved responses; saving them again would only copy files. */
const UNSAVED_TOOLS = new Set(["read_response"]);

function shapeResult(tool: string, args: Fields | undefined, content: Fields, isError: boolean, saved: boolean) {
  // A preview is the routing decision itself, so it always stays whole. Without a saved file
  // nothing may move out of the view: there would be nowhere left to read it.
  const full = args?.receipt === "full" || args?.preview === true || !saved;
  if (tool === "delegate_research") return full ? { shaped: content, sections: [] } : compactResearchResult(content);
  // A deferred acknowledgement carries only the job; its result arrives through task_jobs.
  if (tool !== "run_task" || isError || content.deferred === true) return { shaped: content, sections: [] };
  let shaped = full ? content : compactTaskResult(content);
  const sections = full ? [] : ["receipt"];
  if (!full && !Array.isArray(content.results) && withoutThinking(object(content.response))) sections.push("thinking");
  if (args?.returnEmbeddings !== true) {
    const trimmed = summarizeEmbeddings(shaped, { saved });
    if (trimmed) {
      shaped = trimmed;
      if (saved) sections.push("embeddings");
    }
  }
  return { shaped, sections };
}

/** The model a result came from, for the store index. */
function resultModel(content: Fields | undefined): string | undefined {
  const model = content?.selected_model ?? object(content?.route)?.selected_model;
  return typeof model === "string" && model ? model : undefined;
}

/**
 * One tool result as the agent receives it: timed, saved whole to the response store (the path
 * travels as `response_file`), and for a successful `run_task` or any `delegate_research` reduced
 * to what the agent acts on unless the call asked for `receipt: "full"`. `sections` names what
 * the file holds beyond that view (read_response returns one). The answer is never shortened.
 */
export async function finishResult<T extends ToolResult>(
  tool: string,
  args: unknown,
  result: T,
  elapsedMs: number,
  startedAt: string,
  store: Pick<ResponseStore, "save"> = responseStore,
): Promise<T> {
  const timed = withTiming(result, elapsedMs, startedAt);
  const content = object(timed.structuredContent);
  const file = UNSAVED_TOOLS.has(tool)
    ? undefined
    : await store.save(tool, { tool, arguments: args, result: timed }, { model: resultModel(content) });
  if (!content) {
    return file ? { ...timed, _meta: { ...timed._meta, "freellama/response_file": file } } : timed;
  }
  const { shaped, sections } = shapeResult(tool, object(args), content, timed.isError === true, file !== undefined);
  return {
    ...timed,
    structuredContent: {
      ...shaped,
      timing: content.timing,
      ...(file ? { response_file: file } : {}),
      ...(file && sections.length > 0 ? { sections } : {}),
    },
  };
}

/**
 * Every tool reports how long it took and where its whole result is saved, so an agent sees what a
 * local model, a model switch or a queue cost and can read any receipt in full without carrying
 * it in context. Wraps registration once instead of editing each tool.
 */
export function withToolResults(server: McpServer): McpServer {
  const register = server.registerTool.bind(server);
  server.registerTool = ((name: string, config: unknown, callback: (...args: unknown[]) => unknown) =>
    register(name, config as never, (async (...args: unknown[]) => {
      const started = performance.now();
      const startedAt = new Date().toISOString();
      const result = (await callback(...args)) as ToolResult;
      return finishResult(name, args[0], result, Math.round(performance.now() - started), startedAt);
    }) as never)) as typeof server.registerTool;
  return server;
}
