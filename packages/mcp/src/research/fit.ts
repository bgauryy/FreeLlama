// Context-window fitting shared by the research adapters.
//
// Both adapters run at `num_ctx=8192` and append an observation after every turn. Requests set
// `truncate: false` (managed requests go through FreeLlama, which does the same), so an oversized
// conversation is refused instead of silently front-truncated; either way the run loses the turn.
// `fitToContext` keeps every request inside the window:
//
// - Pinned: the system prompt (index 0) and the question (index 1) are never compacted unless the
//   operator sets FREELLAMA_AGENT_PINNED_OVERFLOW=clip; otherwise an overflow is an explicit error.
// - Compaction: older observations become a deterministic breadcrumb plus a stored-page handle.
// - Pagination (observations.ts): every observation is stored in full and shown page by page.
// - Estimate: when serve has learned the model's bytes-per-token ratio, messages are counted exactly
//   as the platform counts them (`platformTextTokens`). Otherwise the fallback is a character
//   estimate whose multiplier Ollama's `prompt_eval_count` raises, never lowers (`ContextBudgeter`).
import { canonicalJson } from "./canonical-json.js";
import {
  CHARS_PER_TOKEN, CLIP_HEAD_RATIO, COMPACTION_TARGET_RATIO, ContextPolicy, contextBudget, type Message,
} from "./context-policy.js";
import { ValidationError } from "./errors.js";
import { cpLength, cpSlice, cpTail } from "./text.js";
import type { PlatformTokenRatio } from "./transport.js";

export const COMPACT_MARKER = "[earlier observation compacted to save context";

/** Message text as the estimate counts it: Python-style JSON separators, non-ASCII kept. */
const estimateText = (value: unknown) => canonicalJson(value, { sortKeys: false, ensureAscii: false });

/** Approximate a first-call token count; later model counts calibrate the multiplier. */
export function estimateTokens(text: string, charsPerToken: number = CHARS_PER_TOKEN): number {
  return Math.floor(cpLength(text) / charsPerToken) + 1;
}

/** Bytes per token (times 1000) serve assumes for a model it has not measured: 3 bytes/token. */
export const UNMEASURED_BYTES_PER_TOKEN_MILLI = 3000;

/**
 * The ratio the platform estimates with (rust-core `platform/context.rs`): the learned ratio less a
 * 10% margin, so estimates err toward more tokens, clamped to 1–8 bytes per token; 3 bytes per token
 * while unmeasured.
 */
export function platformBytesPerTokenMilli(learnedMilli: number | null | undefined): number {
  if (learnedMilli === null || learnedMilli === undefined) return UNMEASURED_BYTES_PER_TOKEN_MILLI;
  return Math.min(Math.max(Math.floor(learnedMilli / 10) * 9, 1000), 8000);
}

/** The platform's token estimate for `text`: UTF-8 bytes ×1000 / the margined ratio, rounded up. */
export function platformTextTokens(text: string, learnedMilli: number | null | undefined): number {
  return Math.ceil((Buffer.byteLength(text, "utf8") * 1000) / platformBytesPerTokenMilli(learnedMilli));
}

/**
 * Estimate typed Ollama messages without flattening images/tool calls into text. With
 * `learnedMilli` (serve's learned ratio) each message is counted as the platform counts it: its
 * `JSON.stringify` bytes, the form the ratio was learned from, plus one; images keep their fixed
 * charge. Without it the configured characters-per-token estimate applies.
 */
export function messagesTokens(
  messages: Message[], policy: ContextPolicy = new ContextPolicy(), learnedMilli: number | null = null,
): number {
  let total = 0;
  for (const message of messages) {
    const { images = [], ...text } = message;
    if (Array.isArray(images)) total += images.length * policy.image_token_estimate;
    if (learnedMilli !== null) {
      total += platformTextTokens(JSON.stringify(text), learnedMilli) + 1;
      continue;
    }
    const { content = "", ...metadata } = text;
    total += estimateTokens(typeof content === "string" ? content : estimateText(content), policy.chars_per_token);
    total += estimateTokens(estimateText(metadata), policy.chars_per_token);
  }
  return total;
}

/**
 * One token estimator per run. With serve's learned ratio (`platformRatio`) it counts as the
 * platform does and is never rescaled: serve already learns from every managed chat. Otherwise it
 * is the conservative character estimate calibrated by Ollama's real post-call prompt count: each
 * successful call's `prompt_eval_count` raises (never lowers) the multiplier.
 */
export class ContextBudgeter {
  readonly policy: ContextPolicy;
  readonly platformRatio: PlatformTokenRatio | null;
  scale: number;
  calibration_samples: number;

  constructor(
    policy: ContextPolicy = new ContextPolicy(),
    options: { initialScale?: number; calibrationSamples?: number; platformRatio?: PlatformTokenRatio | null } = {},
  ) {
    this.policy = policy;
    this.platformRatio = options.platformRatio ?? null;
    this.scale = this.platformRatio ? 1 : Math.max(options.initialScale ?? 1, 1);
    this.calibration_samples = this.platformRatio ? 0 : Math.max(Math.trunc(options.calibrationSamples ?? 0), 0);
  }

  estimate = (messages: Message[]): number => this.platformRatio
    ? messagesTokens(messages, this.policy, this.platformRatio.bytes_per_token_milli)
    : Math.ceil(messagesTokens(messages, this.policy) * this.scale);

  /** Calibrate the fallback estimate; returns whether the multiplier rose (and should persist). */
  observe(messages: Message[], promptEvalCount: unknown): boolean {
    if (this.platformRatio) return false;
    if (typeof promptEvalCount !== "number" || !Number.isFinite(promptEvalCount)) return false;
    const actual = Math.trunc(promptEvalCount);
    if (actual <= 0) return false;
    const baseline = Math.max(messagesTokens(messages, this.policy), 1);
    const previous = this.scale;
    this.scale = Math.max(this.scale, actual / baseline);
    this.calibration_samples += 1;
    return this.scale > previous;
  }

  metadata(): Record<string, unknown> {
    const ratio = this.platformRatio;
    return {
      token_counting: ratio ? "platform_learned_ratio" : "local_chars_per_token",
      estimate_scale: Math.round(this.scale * 1e6) / 1e6,
      calibration_samples: this.calibration_samples,
      bytes_per_token_milli: ratio?.bytes_per_token_milli ?? null,
      bytes_per_token_milli_with_margin: ratio ? platformBytesPerTokenMilli(ratio.bytes_per_token_milli) : null,
      bytes_per_token_samples: ratio?.bytes_per_token_samples ?? null,
    };
  }
}

/**
 * Truncate to `limit` characters keeping both ends: a majority head (listings, grep results and
 * file reads put their most identifying content first) plus a minority tail.
 */
export function clip(text: string, limit: number, headRatio: number = CLIP_HEAD_RATIO): string {
  if (!(headRatio > 0 && headRatio < 1)) throw new ValidationError("head_ratio must be between 0 and 1");
  const length = cpLength(text);
  if (limit <= 0 || length <= limit) return text;
  const marker = (dropped: number) => `\n… [${dropped} characters elided] …\n`;
  // Reserve room for the marker itself so the result never exceeds `limit`.
  const usable = Math.max(limit - cpLength(marker(length)), 0);
  if (usable === 0) return cpSlice(text, 0, limit);
  const headChars = Math.trunc(usable * headRatio);
  const tailChars = usable - headChars;
  const dropped = length - headChars - tailChars;
  const tail = tailChars ? cpTail(text, tailChars) : "";
  return cpSlice(text, 0, headChars) + marker(dropped) + tail;
}

const STEP_HEADER = /^Observation \(step ([0-9]+)\):\n/;
// The reminder ends the text, optionally followed by one newline.
const LOOP_REMINDER = /\n\n(?:Commands|Tool calls) remaining: [0-9]+\. Finish now if the task is answerable; do not repeat prior (?:commands|calls)\.(?=\n?$)/g;
const PAGE_FOOTER = /\n\n\[page ([0-9]+) of ([0-9]+) — [0-9]+ characters total, nothing discarded\. /;
const decimal = (digits: string) => BigInt(digits).toString();

/**
 * Keep a deterministic evidence breadcrumb and a usable stored-output handle. This is an extract,
 * not a semantic summary; existing breadcrumbs stay byte-identical so later overflows do not
 * invalidate the prefix again.
 */
export function compact(content: string, policy: ContextPolicy): string {
  if (content.startsWith(COMPACT_MARKER)) return content;
  const stepMatch = STEP_HEADER.exec(content);
  let recovery = "";
  let body = content;
  if (stepMatch) {
    const step = decimal(stepMatch[1]!);
    body = content.slice(stepMatch[0].length);
    // The adapter-generated ledger precedes untrusted tool bytes; keep its handles and status.
    const newline = body.indexOf("\n");
    const ledgerLine = newline < 0 ? body : body.slice(0, newline);
    const ledger = ledgerLine.startsWith("[evidence ledger: ") && ledgerLine.endsWith("]") ? ledgerLine : "";
    if (ledger && newline >= 0) body = body.slice(newline + 1);
    // Loop reminders describe the old turn, not the evidence.
    body = body.replace(LOOP_REMINDER, "");
    // Read only the footer shape pageFooter generates, preserving page completeness.
    const footer = PAGE_FOOTER.exec(body);
    const page = footer ? decimal(footer[1]!) : "1";
    const total = footer ? decimal(footer[2]!) : "1";
    if (footer) body = body.slice(0, footer.index);
    recovery = `; step ${step}, page ${page} of ${total}; retrieve stored output: {"action":"page","step":${step},"page":${page}}`;
    if (ledger) recovery += `; ${ledger}`;
  }
  const preview = clip(body, policy.compact_preview_chars, policy.clip_head_ratio);
  return `${COMPACT_MARKER}${recovery}] ${preview}`;
}

const textContent = (message: Message | undefined): string | null => (typeof message?.content === "string" ? message.content : null);
const contentLength = (message: Message | undefined): number => cpLength(textContent(message) ?? "");

/** The first index with the longest content. */
function widestIndex(indexes: number[], messages: Message[]): number | undefined {
  let best: number | undefined;
  let bestLength = -1;
  for (const index of indexes) {
    const length = contentLength(messages[index]);
    if (length > bestLength) {
      best = index;
      bestLength = length;
    }
  }
  return best;
}

function textCandidates(messages: Message[], from: number, to: number, minimum: number): number[] {
  const result: number[] = [];
  for (let index = from; index < to; index += 1) {
    if (textContent(messages[index]) !== null && contentLength(messages[index]) > minimum) result.push(index);
  }
  return result;
}

export interface FitOptions {
  numCtx: number;
  numPredict: number;
  keepRecent?: number;
  policy?: ContextPolicy;
  tokenCounter?: (messages: Message[]) => number;
}

/**
 * Trim a conversation to fit `num_ctx`, protecting the parts that must never be dropped. Returns
 * `[messages, compacted]`. The system prompt (index 0) and question (index 1) are pinned.
 * Compaction walks oldest-first through observation turns, so the model keeps a breadcrumb of
 * every step even when the full text of early ones is gone.
 */
export function fitToContext(messages: Message[], options: FitOptions): [Message[], boolean] {
  const policy = options.policy ?? new ContextPolicy();
  const keepRecent = options.keepRecent ?? policy.keep_recent;
  const budget = contextBudget(options.numCtx, options.numPredict, policy.safety_margin_tokens);
  const count = options.tokenCounter ?? ((value: Message[]) => messagesTokens(value, policy));
  if (count(messages) <= budget) return [messages, false];

  const pinned = Math.min(2, messages.length);
  const trimmed: Message[] = messages.map((message) => ({ ...message }));
  // Tool-result messages are observations too; keep their role and linkage, compact only text.
  const observations: number[] = [];
  for (let index = pinned; index < trimmed.length; index += 1) {
    const role = trimmed[index]!.role;
    if ((role === "user" || role === "tool") && textContent(trimmed[index]) !== null) observations.push(index);
  }
  const protectedIndexes = new Set(keepRecent ? observations.slice(-keepRecent) : []);
  let compacted = false;
  const target = Math.floor(budget * COMPACTION_TARGET_RATIO);
  for (const index of observations) {
    if (count(trimmed) <= target) break;
    if (protectedIndexes.has(index)) continue;
    const content = textContent(trimmed[index])!;
    const replacement = compact(content, policy);
    if (cpLength(replacement) < cpLength(content)) {
      trimmed[index]!.content = replacement;
      compacted = true;
    }
  }

  const shrinkWidest = (candidates: number[]): boolean => {
    const widest = widestIndex(candidates, trimmed);
    if (widest === undefined) return false;
    const content = textContent(trimmed[widest]) ?? "";
    const keep = Math.max(Math.trunc(cpLength(content) * policy.compact_retain_ratio), policy.compact_preview_chars);
    if (keep >= cpLength(content)) return false;
    trimmed[widest]!.content = clip(content, keep, policy.clip_head_ratio);
    compacted = true;
    return true;
  };

  // Still over after compacting every droppable observation: clip the protected ones too. Shrink
  // the largest repeatedly, because the per-message estimate rounds up and "just over" is as
  // silently truncated as "far over".
  for (let guard = 0; count(trimmed) > budget && guard < 200; guard += 1) {
    if (!shrinkWidest(observations)) break;
  }
  // Old reasoning traces can dwarf the answer; reduce them after observations are compacted.
  for (const message of trimmed.slice(pinned, Math.max(trimmed.length - 2, pinned))) {
    const thinking = message.thinking;
    if (count(trimmed) <= budget) break;
    if (typeof thinking === "string" && cpLength(thinking) > policy.compact_preview_chars) {
      message.thinking = compact(thinking, policy);
      compacted = true;
    }
  }
  // Compact any remaining non-pinned textual history before considering the pinned contract.
  for (let guard = 0; count(trimmed) > budget && guard < 200; guard += 1) {
    const candidates = textCandidates(trimmed, pinned, trimmed.length, policy.compact_preview_chars);
    if (candidates.length === 0 || !shrinkWidest(candidates)) break;
  }
  // Pinned text is byte-preserved by default. An operator may explicitly choose `clip`; the safe
  // default fails before Ollama can silently front-truncate the system contract or question.
  for (let guard = 0; policy.pinned_overflow === "clip" && count(trimmed) > budget && guard < 200; guard += 1) {
    const candidates = textCandidates(trimmed, 0, pinned, policy.compact_preview_chars);
    if (candidates.length === 0 || !shrinkWidest(candidates)) break;
  }
  if (count(trimmed) > budget) {
    throw new ValidationError(
      "pinned system/question content and minimum history exceed context budget: "
      + `${count(trimmed)} > ${budget} estimated tokens. Increase FREELLAMA_AGENT_NUM_CTX, `
      + "reduce the task/instructions, or explicitly set FREELLAMA_AGENT_PINNED_OVERFLOW=clip.",
    );
  }
  return [trimmed, compacted];
}
