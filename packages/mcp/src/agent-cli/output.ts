// A tool result as a shell sees it: the answer alone on stdout (pipeable), and on stderr what the
// caller should act on (check: lines) or asked to see (--verbose: model, timings, cache, filled fields).
import type { RenderOptions } from "./args.js";
import { resultText, type ToolResult } from "./registry.js";
import { record } from "./util.js";

type Rendered = { stdout: string; stderr: string[]; exitCode: number };


/** `[model · lifecycle] answer\ncheck: …` -> label, answer lines, check lines. */
export function splitAnswer(text: string): { label?: string; answer: string; checks: string[] } {
  const match = /^\[([^\]\n]*)\] /.exec(text);
  const body = match ? text.slice(match[0].length) : text;
  const lines = body.split("\n");
  const checks = lines.filter((line) => line.startsWith("check: "));
  const answer = lines.filter((line) => !line.startsWith("check: ")).join("\n").trimEnd();
  return { ...(match ? { label: match[1] } : {}), answer, checks };
}

const seconds = (ns: unknown) => (typeof ns === "number" ? `${(ns / 1e9).toFixed(2)} s` : undefined);

/** One line of the receipt's measurements, as `ollama run --verbose` would show them. */
function statsLine(structured: Record<string, unknown>): string | undefined {
  const metrics = record(structured.metrics);
  if (!metrics) return undefined;
  const parts = [
    seconds(metrics.total_duration_ns) && `total ${seconds(metrics.total_duration_ns)}`,
    seconds(metrics.load_duration_ns) && `load ${seconds(metrics.load_duration_ns)}`,
    typeof metrics.prompt_tokens === "number" && `prompt ${metrics.prompt_tokens} tokens${typeof metrics.cached_prompt_tokens === "number" ? ` (${metrics.cached_prompt_tokens} cached)` : ""}`,
    typeof metrics.output_tokens === "number" && `output ${metrics.output_tokens} tokens`,
    typeof metrics.output_tokens_per_second === "number" && `${metrics.output_tokens_per_second.toFixed(1)} tokens/s`,
  ].filter(Boolean);
  const wait = record(structured.admission)?.total_wait_ms;
  if (typeof wait === "number" && wait > 0) parts.push(`waited ${(wait / 1000).toFixed(1)} s for capacity`);
  return parts.length > 0 ? parts.join(", ") : undefined;
}

/** The model's thinking, when the response carries any. */
function thinkingOf(structured: Record<string, unknown>): string | undefined {
  const thinking = record(record(structured.response)?.message)?.thinking;
  return typeof thinking === "string" && thinking.trim() !== "" ? thinking.trim() : undefined;
}

/**
 * Render one tool result. `showThinking` is the caller's terminal default (stderr is a TTY);
 * --hidethinking always wins. Errors go to stderr with exit code 1.
 */
export function renderResult(result: ToolResult, options: RenderOptions, showThinking = false): Rendered {
  const structured = result.structuredContent ?? {};
  if (result.isError) {
    // --json keeps the refusal's fields (code, picks, what holds memory) for a script to read.
    if (options.json) return { stdout: resultText(result), stderr: [], exitCode: 1 };
    return { stdout: "", stderr: [refusalOf(resultText(result)).message], exitCode: 1 };
  }
  if (options.json) return { stdout: JSON.stringify(result.structuredContent ?? { text: resultText(result) }, null, 2), stderr: [], exitCode: 0 };
  const stderr: string[] = [];
  const job = record(structured.job);
  if (structured.deferred === true && typeof job?.id === "string") {
    if (!options.quiet) stderr.push(`deferred; wait for it with: fllama job ${job.id} --wait`);
    return { stdout: job.id, stderr, exitCode: 0 };
  }
  const { label, answer: body, checks } = splitAnswer(resultText(result));
  // A saved handoff's path is a note, not part of the answer a pipe should carry.
  const handoff = body.split("\n").filter((line) => line.startsWith("handoff_file: "));
  const answer = body.split("\n").filter((line) => !line.startsWith("handoff_file: ")).join("\n").trimEnd();
  if (!options.quiet) stderr.push(...handoff);
  if (options.verbose) {
    if (label) stderr.push(`[${label}]`);
    const profile = record(structured.job_profile);
    if (profile) stderr.push(`job ${String(profile.job)}${profile.workflow ? `/${String(profile.workflow)}` : ""}: model ${String(profile.model)}; filled ${(profile.applied as string[] | undefined)?.join(", ") || "nothing"}`);
    const stats = statsLine(structured);
    if (stats) stderr.push(stats);
  }
  // An answer cut at the output limit reads complete but is not.
  if (record(structured.response)?.done_reason === "length" && !options.quiet) stderr.push("fllama: the answer stopped at the output limit; raise -o num_predict=N for the rest");
  // An embedding run prints its vectors (`ollama run EMBEDDING_MODEL "text"` does).
  const vectors = record(structured.response)?.embeddings;
  if (Array.isArray(vectors) && vectors.length > 0) return { stdout: JSON.stringify(vectors.length === 1 ? vectors[0] : vectors), stderr, exitCode: 0 };
  const thinking = thinkingOf(structured);
  if (thinking && !options.hideThinking && (options.verbose || showThinking) && !options.quiet) stderr.push(`thinking: ${thinking}`);
  if (!options.quiet) stderr.push(...checks);
  // A thinking model that spent the whole budget answers nothing: that is a failure for a script.
  if (/^\(empty (answer|response)/.test(answer)) {
    const hint = answer.includes("thinking") ? "the model spent its output thinking; rerun with --think=false or raise -o num_predict=N" : "the model returned nothing";
    return { stdout: "", stderr: [...stderr, `fllama: empty answer: ${hint}`], exitCode: 1 };
  }
  if (options.extract) {
    const block = firstCodeBlock(answer);
    if (block === undefined) {
      stderr.push("fllama: --extract found no code block; printing the whole answer");
      return { stdout: answer, stderr, exitCode: 0 };
    }
    return { stdout: block, stderr, exitCode: 0 };
  }
  return { stdout: answer, stderr, exitCode: 0 };
}

/** The body of the first fenced code block (``` or ~~~), without its fences and language tag. */
function firstCodeBlock(text: string): string | undefined {
  const match = /^(`{3,}|~{3,})[^\n]*\n([\s\S]*?)^\1[ \t]*$/m.exec(text);
  return match ? match[2]!.replace(/\n$/, "") : undefined;
}

/**
 * A tool's error text as a client should read it: the `error` of a JSON refusal (with its code),
 * else the text. `unavailable` marks what retrying later can fix: no capacity, no memory, no
 * Ollama, no pick that fits now. Those are 503s; the rest are failures.
 */
export function refusalOf(text: string): { message: string; code?: string; unavailable: boolean } {
  let message = text.trim() || "the task failed";
  let code: string | undefined;
  try {
    const parsed = record(JSON.parse(message));
    if (typeof parsed?.error === "string") {
      code = typeof parsed.code === "string" ? parsed.code : undefined;
      message = code ? `${parsed.error} (${code})` : parsed.error;
    }
  } catch {
    // Plain text.
  }
  const unavailable = /admission|memory|capacity|queue|fits now|no catalog pick|inventory_unavailable|no_model_fits|not running|unavailable/i.test(`${code ?? ""} ${message}`);
  return { message, ...(code ? { code } : {}), unavailable };
}

