// `freellama run` on a terminal: tokens stream as Ollama generates them, the way `ollama run` shows
// them, while the request still gets FreeLlama's handling: the job's pick and prompts (or a direct
// tag), the loaded runner's window, serve's context-shift setting and managed admission for a cold
// load (virtual.ts prepareForward). Checks and stats follow on stderr. Tasks a stream cannot carry
// (decisions, embeddings, tools, batches, deferred or saved results) stay on the managed path.
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { applyTaskFiles } from "../task-files.js";
import type { RenderOptions } from "./args.js";
import { EXIT_RETRY } from "./output.js";
import { record, type Fields } from "./util.js";
import { directModel, GatewayError, prepareForward, streamChecks, virtualModel, type VirtualModel } from "./virtual.js";

type Io = { out: (text: string) => void; err: (text: string) => void; write: (text: string) => void };

/** The model a streamed run resolves, or undefined when the task needs the managed path. */
export function streamTarget(args: Fields): VirtualModel | undefined {
  // Fill in the middle runs on the named model only.
  if (args.suffix !== undefined) return typeof args.model === "string" ? directModel(args.model) : undefined;
  if (["tools", "defer", "preview", "saveAs", "questions", "state", "input", "nearest", "sessionId", "scopeId", "fallbackModels", "idempotencyKey"].some((field) => args[field] !== undefined)) return undefined;
  if (args.task === "decision" || args.task === "embedding") return undefined;
  if (typeof args.workflow === "string") return virtualModel(`freellama/${args.workflow}`);
  if (typeof args.job === "string") return virtualModel(`freellama/${args.job}`);
  if (typeof args.model === "string") return directModel(args.model, args.task === "vision" ? "vision" : typeof args.task === "string" ? args.task : "completion");
  return virtualModel("freellama/auto");
}

const seconds = (ns: unknown) => (typeof ns === "number" ? `${(ns / 1e9).toFixed(2)} s` : "?");

/** Ollama's done-chunk numbers as one line, `ollama run --verbose` style. */
function doneStats(done: Fields): string {
  const evalRate = typeof done.eval_count === "number" && typeof done.eval_duration === "number" && done.eval_duration > 0
    ? `, ${(done.eval_count / (done.eval_duration / 1e9)).toFixed(1)} tokens/s` : "";
  const cached = typeof done.prompt_eval_cached_count === "number" ? ` (${done.prompt_eval_cached_count} cached)` : "";
  return `total ${seconds(done.total_duration)}, load ${seconds(done.load_duration)}, prompt ${String(done.prompt_eval_count ?? "?")} tokens${cached}, output ${String(done.eval_count ?? "?")} tokens${evalRate}`;
}

/**
 * Stream one task to the terminal. Returns the exit code: 0 answered, 1 refused, failed or empty,
 * 130 interrupted.
 */
export async function streamRun(target: VirtualModel, args: Fields, render: RenderOptions, io: Io, signal: AbortSignal, showThinking: boolean): Promise<number> {
  const endpoint = typeof args.endpoint === "string" ? args.endpoint : undefined;
  // Files are read exactly as run_task reads them: text under the prompt, images alongside.
  const { payload } = applyTaskFiles({ task: target.vision ? "vision" : "completion", prompt: args.prompt as string | undefined }, args.files as string[] | undefined);
  const images = (payload as { images?: string[] }).images;
  const options = { ...record(args.options), ...(typeof args.contextTokens === "number" ? { num_ctx: args.contextTokens } : {}) };
  // Fill in the middle is a completion between `prompt` and `suffix` on /api/generate; every
  // other run is a chat.
  const fill = typeof args.suffix === "string";
  const path = fill ? "/api/generate" : "/api/chat";
  const body: Fields = {
    model: target.name,
    stream: true,
    ...(fill ? { prompt: payload.prompt ?? "", suffix: args.suffix } : {
      messages: [
        ...(typeof args.systemPrompt === "string" ? [{ role: "system", content: args.systemPrompt }] : []),
        { role: "user", content: payload.prompt ?? "", ...(images?.length ? { images } : {}) },
      ],
    }),
    ...(Object.keys(options).length > 0 ? { options } : {}),
    ...(args.think !== undefined ? { think: args.think } : {}),
    ...(args.format !== undefined ? { format: args.format } : {}),
    ...(args.keepAlive !== undefined ? { keep_alive: args.keepAlive } : {}),
  };
  let prepared;
  try {
    prepared = await prepareForward(path, target, body, endpoint, signal);
  } catch (error) {
    if (signal.aborted) return 130;
    io.err(error instanceof GatewayError ? error.message : `freellama: ${(error as Error).message}`);
    // A refusal waiting can fix (busy, memory, Ollama down) is retryable.
    if (error instanceof GatewayError && error.status === 503) {
      io.err(`freellama: retryable (exit ${EXIT_RETRY})`);
      return EXIT_RETRY;
    }
    return 1;
  }
  const { resolution } = prepared;
  if (render.verbose) io.err(`[${resolution.model} · num_ctx ${prepared.numCtx ?? "default"} from ${resolution.contextSource ?? "Ollama"}${prepared.runner === "warmed" ? " · loaded through admission" : ""}]`);
  let reply: Response;
  try {
    reply = await fetch(new URL(path, endpoint ?? DEFAULT_SERVE_ENDPOINT), {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(prepared.body), signal,
    });
  } catch (error) {
    if (signal.aborted) return 130;
    io.err(`freellama: serve did not answer: ${(error as Error).message}`);
    return 1;
  }
  if (!reply.ok || !reply.body) {
    const text = await reply.text().catch(() => "");
    io.err(`freellama: ${String(record(safeJson(text))?.error ?? (text || `HTTP ${reply.status}`))}`);
    return 1;
  }
  const decoder = new TextDecoder();
  let pending = "";
  let answer = "";
  let thinking = false;
  let toolCalled = false;
  let done: Fields | undefined;
  const handle = (line: string) => {
    const chunk = record(safeJson(line));
    if (!chunk) return;
    if (typeof chunk.error === "string") throw new Error(chunk.error);
    const message = fill ? { content: chunk.response, thinking: chunk.thinking } : record(chunk.message);
    const thought = message?.thinking;
    if (typeof thought === "string" && thought !== "" && showThinking && !render.hideThinking && !render.quiet) {
      if (!thinking) io.err("thinking:");
      thinking = true;
      process.stderr.write(thought);
    }
    if (Array.isArray(message?.tool_calls) && message.tool_calls.length > 0) toolCalled = true;
    const piece = message?.content;
    if (typeof piece === "string" && piece !== "") {
      if (thinking) process.stderr.write("\n");
      thinking = false;
      answer += piece;
      io.write(piece);
    }
    if (chunk.done === true) done = chunk;
  };
  try {
    for await (const bytes of reply.body as unknown as AsyncIterable<Uint8Array>) {
      pending += decoder.decode(bytes, { stream: true });
      const lines = pending.split("\n");
      pending = lines.pop() ?? "";
      lines.forEach(handle);
    }
    handle(pending + decoder.decode());
  } catch (error) {
    if (signal.aborted) {
      io.write("\n");
      return 130;
    }
    io.err(`freellama: ${(error as Error).message}`);
    return 1;
  }
  if (answer !== "" && !answer.endsWith("\n")) io.write("\n");
  if (answer.trim() === "") {
    if (toolCalled) {
      io.err("freellama: no answer: the model replied with a tool call although no tools were sent; rerun with -m on a stronger installed model (freellama catalog)");
      return 1;
    }
    const why = done?.done_reason === "length" ? "the output budget ran out (thinking?); rerun with --think=false or raise -o num_predict=N" : "the model returned nothing";
    io.err(`freellama: empty answer: ${why}`);
    return 1;
  }
  if (done?.done_reason === "length" && !render.quiet) io.err("freellama: the answer stopped at the output limit; raise -o num_predict=N for the rest");
  if (!render.quiet) for (const check of streamChecks(resolution, answer)) io.err(`check: ${check}`);
  if (render.verbose && done) io.err(doneStats(done));
  return 0;
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}
