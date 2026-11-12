import * as z from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { summarizeEmbeddings } from "./helpers.js";
import { responseStore, type ResponseStore } from "./response-store.js";
import { summaryOf, writeDefaultOutput, writeOutputFile } from "./handoffs.js";

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
  // A plan that changed nothing and weighed no candidate says nothing an agent acts on.
  if (!planning || (planning.applied !== true && !(Array.isArray(planning.candidates) && planning.candidates.length > 0)
    && !(Array.isArray(planning.payload_fit_rejections) && planning.payload_fit_rejections.length > 0))) return undefined;
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
function compactReceipt(receipt: Fields, item = false): Fields {
  const execution = object(receipt.execution);
  const reservation = pick(object(execution?.memory_reservation), ["evicted_idle_models", "eviction", "required_available_bytes", "source",
    // Ollama's own fit check predicts an f16 cache; when that alone fails, it may evict or offload.
    "ollama_may_evict_or_offload", "ollama_preflight_reason", "ollama_f16_preflight_bytes"]);
  const completionPlanning = compactCompletionPlanning(execution?.completion_planning);
  const compact: Fields = {};
  for (const key of ["selected_model", "response", "output_check", "scope", "warm", "job", "deferred",
    "nearest", "files_read", "embedding_requests", "embedding_identity", "accepted_job_ids", "job_profile", "handoff_file"]) {
    if (receipt[key] !== undefined) compact[key] = receipt[key];
  }
  const response = withoutThinking(object(receipt.response));
  if (response) compact.response = response;
  // Host telemetry snapshots stay in the file; metrics keep their scalar counts and durations.
  const metrics = object(receipt.metrics);
  if (metrics) compact.metrics = Object.fromEntries(Object.entries(metrics).filter(([, value]) => value === null || typeof value !== "object"));
  const route = pick(object(receipt.route), ["selected_model", "task", "confidence", "reasons"]);
  if (route) compact.route = route;
  // A batch item keeps its answer, cost, model and any completion plan; the rest of where and how
  // each item ran is in the file.
  if (item) {
    const planned = pick(execution, ["backend", "reason"]);
    if (execution && (planned || completionPlanning)) compact.execution = { ...planned, ...(completionPlanning ? { completion_planning: completionPlanning } : {}) };
    return compact;
  }
  const admission = pick(object(receipt.admission), ["mode", "queue_wait_ms", "transition_wait_ms", "resource_wait_ms", "total_wait_ms"]);
  if (admission) compact.admission = admission;
  if (execution) {
    // The physical placement and how long the runner stays: their derivations stay in the file.
    const observation = pick(object(execution.observation), ["status", "processor", "context_length"]);
    const keepAlive = pick(object(execution.keep_alive), ["value", "reason"]);
    compact.execution = {
      ...pick(execution, ["placement", "backend", "reason", "preference_satisfied"]),
      ...(observation ? { observation } : {}),
      ...(keepAlive ? { keep_alive: keepAlive } : {}),
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
        return item && response ? { ...item, response: compactReceipt(response, true) } : row;
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

/** The finished job's result of a `task_jobs {action:"get"}` payload, when it has one. */
function finishedJob(tool: string, args: Fields | undefined, content: Fields): { job: Fields; result: Fields } | undefined {
  if (tool !== "task_jobs" || args?.action !== "get") return undefined;
  const job = object(content.job);
  const result = object(job?.result);
  return job && result ? { job, result } : undefined;
}

function shapeResult(tool: string, args: Fields | undefined, content: Fields, isError: boolean, saved: boolean) {
  // A preview is the routing decision itself, so it always stays whole. Without a saved file
  // nothing may move out of the view: there would be nowhere left to read it.
  const full = args?.receipt === "full" || args?.preview === true || !saved;
  // A finished job's receipt is the one `run_task` would have returned: shape it the same way.
  const finished = finishedJob(tool, args, content);
  if (finished) {
    if (full || args?.returnEmbeddings === true) return { shaped: content, sections: [] };
    return { shaped: { ...content, job: { ...finished.job, result: compactTaskResult(finished.result) } }, sections: ["receipt"] };
  }
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

/**
 * Longest answer text `output: "auto"` returns inline: about 2,000 tokens. Claude Code warns above
 * 10,000 tokens of MCP output and caps it at 25,000 by default (code.claude.com/docs/en/mcp);
 * other hosts cut sooner.
 */
export const AUTO_INLINE_CHARS = 8000;

/** Input fields of the tools whose answers `output` and `outputPath` apply to. */
export const outputInput = {
  output: z.enum(["auto", "inline", "file"]).optional()
    .describe("auto (default): inline up to 8,000 chars, else a summary and where the rest is"),
  outputPath: z.string().min(1).optional()
    .describe("also write the answer here as Markdown (inside the readable roots)"),
};

/** The saved record's path to the whole answer text: `{tool, arguments, result: {content: [{text}]}}`. */
export const SAVED_TEXT_SECTION = "/result/content/0/text";

const KEPT_LINE = /^(check|handoff_file|verification|cited): /;
const LABEL = /^\[[^\]\n]*\] /;

/**
 * The short view of an answer text: its `[model · ...]` label, the opening lines, every `check:`,
 * `handoff_file:`, `verification:` and `cited:` line unchanged, then `where` the whole text can be read.
 */
export function shortView(text: string, where: string): string {
  const label = LABEL.exec(text)?.[0].trim();
  const lines = text.replace(LABEL, "").split("\n");
  const kept = lines.filter((line) => KEPT_LINE.test(line));
  const summary = summaryOf(lines.filter((line) => !KEPT_LINE.test(line)).join("\n"));
  return [label, summary, ...kept, where].filter((part) => part !== undefined && part !== "").join("\n");
}

type OutputView = {
  text: string;
  output?: { mode: string; chars: number; file: string };
  output_file?: string;
  /** Where the whole answer can be read, set when the view was shortened. */
  where?: string;
  /** The whole text, for finding its copies in the structured result. */
  answer?: string;
};

/**
 * The text of a finished `run_task` or `delegate_research` result for the call's `output` and
 * `outputPath`. The caller's `outputPath` is written (created or replaced) whatever the mode; a
 * shortened view without one is written to a new file in OUTPUT_TMP_DIR. An answer never disappears:
 * it is shortened only when the whole text is readable in a file or the saved response, and a failed
 * write is named in the text.
 */
function viewText(args: Fields | undefined, text: string, responseFile: string | undefined): OutputView {
  const mode = typeof args?.output === "string" ? args.output : "auto";
  const shorten = mode === "file" || (mode === "auto" && text.length > AUTO_INLINE_CHARS);
  const answer = text.replace(LABEL, "");
  const content = { source: "From FreeLlama", summary: summaryOf(answer), result: answer };
  let written: string | undefined;
  const notes: string[] = [];
  try {
    if (typeof args?.outputPath === "string") written = writeOutputFile(args.outputPath, content);
    else if (shorten) written = writeDefaultOutput(content);
  } catch (error) {
    notes.push(`output_file not written: ${error instanceof Error ? error.message : String(error)}`);
  }
  const file = written ?? (shorten ? responseFile : undefined);
  const tail = notes.length > 0 ? `\n${notes.join("\n")}` : "";
  if (!shorten) {
    return { text: `${text}${written ? `\noutput_file: ${written}` : ""}${tail}`, ...(written ? { output_file: written } : {}) };
  }
  if (!file) return { text: `${text}${tail}\nnot shortened (${text.length} chars): no output file could be written and the response store is off, so this text is the only copy` };
  const where = written
    ? `output_file: ${written} (the full answer, ${text.length} chars)`
    : `full answer (${text.length} chars): read_response{file:"${responseFile}", section:"${SAVED_TEXT_SECTION}"}`;
  return {
    text: `${shortView(text, where)}${tail}`,
    output: { mode, chars: text.length, file },
    ...(written ? { output_file: written } : {}),
    where,
    answer: text,
  };
}

/** Copies shorter than this are never replaced: they are labels and one-liners, not the answer. */
const MIN_COPY_CHARS = 400;

/**
 * `value` with every long string that is the shortened answer, or a piece of it (a batch answer
 * holds one section per item), replaced by `pointer`. The saved response keeps them.
 */
function withoutAnswerCopies(value: unknown, answer: string, pointer: string): unknown {
  if (typeof value === "string") return value.length >= MIN_COPY_CHARS && answer.includes(value.trim()) ? pointer : value;
  if (Array.isArray(value)) return value.map((item) => withoutAnswerCopies(item, answer, pointer));
  const fields = object(value);
  return fields ? Object.fromEntries(Object.entries(fields).map(([key, item]) => [key, withoutAnswerCopies(item, answer, pointer)])) : value;
}

/** `content` with its first text part replaced, or undefined when the result has no text to reshape. */
function reshapedContent(result: ToolResult & { content?: unknown }, args: Fields | undefined, responseFile: string | undefined) {
  const parts = result.content;
  const first = Array.isArray(parts) ? object(parts[0]) : undefined;
  if (!Array.isArray(parts) || first?.type !== "text" || typeof first.text !== "string") return undefined;
  const view = viewText(args, first.text, responseFile);
  return { view, content: [{ ...first, text: view.text }, ...parts.slice(1)] };
}

/** Results whose answer text `output` and `outputPath` apply to: finished, not routing or acknowledgements. */
function hasAnswerText(tool: string, args: Fields | undefined, content: Fields, isError: boolean): boolean {
  if (isError) return false;
  if (finishedJob(tool, args, content)) return true;
  if (tool !== "run_task" && tool !== "delegate_research") return false;
  return args?.preview !== true && args?.defer !== true && content.deferred !== true;
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
 * the file holds beyond that view (read_response returns one). The answer text shortens only as
 * `output` says (see viewText), and only when its whole text is readable elsewhere.
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
  const reshaped = hasAnswerText(tool, object(args), content, timed.isError === true)
    ? reshapedContent(timed as ToolResult & { content?: unknown }, object(args), file)
    : undefined;
  const { where, answer } = reshaped?.view ?? {};
  const lean = where !== undefined && answer !== undefined ? withoutAnswerCopies(shaped, answer, `answer moved: ${where}`) as Fields : shaped;
  return {
    ...timed,
    ...(reshaped ? { content: reshaped.content } : {}),
    structuredContent: {
      ...lean,
      timing: content.timing,
      ...(file ? { response_file: file } : {}),
      ...(file && sections.length > 0 ? { sections } : {}),
      ...(reshaped?.view.output ? { output: reshaped.view.output } : {}),
      ...(reshaped?.view.output_file ? { output_file: reshaped.view.output_file } : {}),
    },
  };
}

/**
 * Tools whose answer is text (no outputSchema): hosts that show structuredContent instead of the
 * text (Claude Code does) would hand the agent the whole receipt, 1-3k tokens around a two-line
 * answer, measured 2026-11-12. These answer with the text alone; the receipt stays in
 * response_file. `receipt:"full"` or FREELLAMA_MCP_RECEIPT=structured keeps it inline.
 */
const TEXT_ANSWER_TOOLS = new Set(["run_task", "task_jobs"]);

/** Whether answer tools answer with text (the default) or keep the receipt inline. */
const textAnswers = (): boolean => process.env.FREELLAMA_MCP_RECEIPT !== "structured";

/** The result an MCP host shows its agent: the answer text and where the receipt is, for answer tools. */
export function agentView<T extends ToolResult>(tool: string, args: unknown, result: T): T {
  const structured = object(result.structuredContent);
  if (!TEXT_ANSWER_TOOLS.has(tool) || !structured || result.isError === true) return result;
  if (object(args)?.receipt === "full" || !textAnswers()) return result;
  const file = typeof structured.response_file === "string" ? structured.response_file : undefined;
  const parts = (result as { content?: Array<{ type: string; text?: string }> }).content ?? [];
  const index = parts.findIndex((part) => part.type === "text");
  const pointer = file ? `response_file: ${file}` : undefined;
  const content = index < 0
    ? [...parts, ...(pointer ? [{ type: "text", text: pointer }] : [])]
    : parts.map((part, at) => (at === index && pointer && !(part.text ?? "").includes(pointer) ? { ...part, text: `${part.text ?? ""}\n${pointer}` } : part));
  const { structuredContent: _dropped, ...rest } = result as T & { structuredContent?: unknown };
  const meta = { ...(result as { _meta?: Record<string, unknown> })._meta, ...(file ? { "freellama/response_file": file } : {}), ...(structured.timing ? { "freellama/timing": structured.timing } : {}) };
  return { ...rest, content, _meta: meta } as unknown as T;
}

/**
 * Every tool reports how long it took and where its whole result is saved, so an agent sees what a
 * local model, a model switch or a queue cost and can read any receipt in full without carrying
 * it in context. Wraps registration once instead of editing each tool.
 */
export function withToolResults(server: McpServer): McpServer {
  const register = server.registerTool.bind(server);
  const text = textAnswers();
  server.registerTool = ((name: string, config: unknown, callback: (...args: unknown[]) => unknown) =>
    // An output schema obliges every result to carry structuredContent; a text answer tool drops it.
    register(name, (text && TEXT_ANSWER_TOOLS.has(name) ? { ...(config as object), outputSchema: undefined } : config) as never, (async (...args: unknown[]) => {
      const started = performance.now();
      const startedAt = new Date().toISOString();
      const result = (await callback(...args)) as ToolResult;
      return agentView(name, args[0], await finishResult(name, args[0], result, Math.round(performance.now() - started), startedAt));
    }) as never)) as typeof server.registerTool;
  return server;
}
