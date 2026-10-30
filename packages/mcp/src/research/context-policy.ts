// Validated context and runtime settings for the research adapters, read from FREELLAMA_AGENT_* env.
import { ValidationError } from "./errors.js";

export type Message = Record<string, unknown>;
export type Env = Record<string, string | undefined>;

export const CHARS_PER_TOKEN = 4;
export const SAFETY_MARGIN_TOKENS = 256;
export const IMAGE_TOKEN_ESTIMATE = 1024;
export const DEFAULT_KEEP_RECENT = 2;
export const COMPACT_PREVIEW_CHARS = 180;
export const COMPACT_RETAIN_RATIO = 0.8;
export const CLIP_HEAD_RATIO = 2 / 3;
export const OBSERVATION_PAGE_CHARS = 3000;
export const COMPACTION_TARGET_RATIO = 0.9;
// Unparseable turns repaired before the run gives up; a model told explicitly usually corrects.
export const MAX_PARSE_REPAIRS = 2;

const INTEGER = /^[+-]?[0-9]+$/;
const DECIMAL = /^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/;

function envInt(env: Env, name: string, defaultValue: number, minimum = 0): number {
  const raw = env[name];
  if (raw === undefined || raw === "") return defaultValue;
  if (!INTEGER.test(raw.trim())) throw new ValidationError(`${name} must be an integer, got ${JSON.stringify(raw)}`);
  const value = Number(raw.trim());
  if (value < minimum) throw new ValidationError(`${name} must be >= ${minimum}, got ${value}`);
  return value;
}

function envFloat(env: Env, name: string, defaultValue: number, minimum = 0): number {
  const raw = env[name];
  if (raw === undefined || raw === "") return defaultValue;
  const value = Number(raw.trim());
  if (!DECIMAL.test(raw.trim())) throw new ValidationError(`${name} must be a number, got ${JSON.stringify(raw)}`);
  if (!Number.isFinite(value) || value < minimum) {
    throw new ValidationError(`${name} must be finite and >= ${minimum}, got ${JSON.stringify(raw)}`);
  }
  return value;
}

function envBool(env: Env, name: string, defaultValue: boolean): boolean {
  const raw = env[name];
  if (raw === undefined || raw === "") return defaultValue;
  const normalized = raw.trim().toLowerCase();
  if (["1", "true", "yes", "on"].includes(normalized)) return true;
  if (["0", "false", "no", "off"].includes(normalized)) return false;
  throw new ValidationError(`${name} must be true/false, got ${JSON.stringify(raw)}`);
}

/** Tokens available for the conversation itself, once generation and slack are reserved. */
export function contextBudget(numCtx: number, numPredict: number, safetyMarginTokens = SAFETY_MARGIN_TOKENS): number {
  return Math.max(numCtx - numPredict - safetyMarginTokens, 0);
}

export interface ContextPolicyFields {
  chars_per_token: number;
  safety_margin_tokens: number;
  image_token_estimate: number;
  keep_recent: number;
  compact_preview_chars: number;
  compact_retain_ratio: number;
  clip_head_ratio: number;
  observation_page_chars: number;
  pinned_overflow: string;
}

/** Validated context-management policy shared by every research adapter. */
export class ContextPolicy {
  readonly fields: ContextPolicyFields;
  /**
   * Fields configured from the environment. They are floats, so canonical JSON (calibration keys)
   * renders them as floats: `FREELLAMA_AGENT_CHARS_PER_TOKEN=8` gives `8.0`, the default gives `4`.
   */
  readonly floatKeys: ReadonlySet<string>;

  constructor(fields: Partial<ContextPolicyFields> = {}, floatKeys: Iterable<string> = []) {
    this.fields = {
      chars_per_token: fields.chars_per_token ?? CHARS_PER_TOKEN,
      safety_margin_tokens: fields.safety_margin_tokens ?? SAFETY_MARGIN_TOKENS,
      image_token_estimate: fields.image_token_estimate ?? IMAGE_TOKEN_ESTIMATE,
      keep_recent: fields.keep_recent ?? DEFAULT_KEEP_RECENT,
      compact_preview_chars: fields.compact_preview_chars ?? COMPACT_PREVIEW_CHARS,
      compact_retain_ratio: fields.compact_retain_ratio ?? COMPACT_RETAIN_RATIO,
      clip_head_ratio: fields.clip_head_ratio ?? CLIP_HEAD_RATIO,
      observation_page_chars: fields.observation_page_chars ?? OBSERVATION_PAGE_CHARS,
      pinned_overflow: fields.pinned_overflow ?? "error",
    };
    this.floatKeys = new Set(floatKeys);
    const f = this.fields;
    if (!Number.isFinite(f.chars_per_token) || f.chars_per_token <= 0) throw new ValidationError("chars_per_token must be finite and > 0");
    if (f.safety_margin_tokens < 0 || f.image_token_estimate < 0 || f.keep_recent < 0) {
      throw new ValidationError("context token estimates and keep_recent must be >= 0");
    }
    if (f.compact_preview_chars <= 0 || f.observation_page_chars <= 0) throw new ValidationError("preview and page sizes must be > 0");
    if (!(f.compact_retain_ratio > 0 && f.compact_retain_ratio < 1)) throw new ValidationError("compact_retain_ratio must be between 0 and 1");
    if (!(f.clip_head_ratio > 0 && f.clip_head_ratio < 1)) throw new ValidationError("clip_head_ratio must be between 0 and 1");
    if (f.pinned_overflow !== "error" && f.pinned_overflow !== "clip") throw new ValidationError("pinned_overflow must be 'error' or 'clip'");
  }

  get chars_per_token() { return this.fields.chars_per_token; }
  get safety_margin_tokens() { return this.fields.safety_margin_tokens; }
  get image_token_estimate() { return this.fields.image_token_estimate; }
  get keep_recent() { return this.fields.keep_recent; }
  get compact_preview_chars() { return this.fields.compact_preview_chars; }
  get compact_retain_ratio() { return this.fields.compact_retain_ratio; }
  get clip_head_ratio() { return this.fields.clip_head_ratio; }
  get observation_page_chars() { return this.fields.observation_page_chars; }
  get pinned_overflow() { return this.fields.pinned_overflow; }

  static fromEnv(env: Env = process.env): ContextPolicy {
    const floats: Array<[keyof ContextPolicyFields, string]> = [
      ["chars_per_token", "FREELLAMA_AGENT_CHARS_PER_TOKEN"],
      ["compact_retain_ratio", "FREELLAMA_AGENT_COMPACT_RETAIN_RATIO"],
      ["clip_head_ratio", "FREELLAMA_AGENT_CLIP_HEAD_RATIO"],
    ];
    return new ContextPolicy({
      chars_per_token: envFloat(env, "FREELLAMA_AGENT_CHARS_PER_TOKEN", CHARS_PER_TOKEN, 0.000001),
      safety_margin_tokens: envInt(env, "FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS", SAFETY_MARGIN_TOKENS),
      image_token_estimate: envInt(env, "FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE", IMAGE_TOKEN_ESTIMATE),
      keep_recent: envInt(env, "FREELLAMA_AGENT_KEEP_RECENT", DEFAULT_KEEP_RECENT),
      compact_preview_chars: envInt(env, "FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS", COMPACT_PREVIEW_CHARS, 1),
      compact_retain_ratio: envFloat(env, "FREELLAMA_AGENT_COMPACT_RETAIN_RATIO", COMPACT_RETAIN_RATIO, 0.000001),
      clip_head_ratio: envFloat(env, "FREELLAMA_AGENT_CLIP_HEAD_RATIO", CLIP_HEAD_RATIO, 0.000001),
      observation_page_chars: envInt(env, "FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS", OBSERVATION_PAGE_CHARS, 1),
      pinned_overflow: (env.FREELLAMA_AGENT_PINNED_OVERFLOW ?? "error").trim().toLowerCase(),
    }, floats.filter(([, name]) => env[name] !== undefined && env[name] !== "").map(([key]) => key));
  }

  metadata(): Record<string, unknown> {
    return { ...this.fields };
  }
}

export interface AgentRuntimeFields {
  max_turns: number;
  num_ctx: number;
  num_predict: number;
  temperature: number;
  seed: number;
  request_timeout_seconds: number;
  retry_attempts: number;
  retry_backoff_seconds: number;
  tool_timeout_seconds: number;
  keep_alive: string;
  think: boolean;
  max_parse_repairs: number;
  parse_repair_echo_chars: number;
  context: ContextPolicy;
}

/** Typed operational settings; safety confinement remains an invariant, not a knob. */
export class AgentRuntimeConfig {
  constructor(readonly fields: AgentRuntimeFields) {}

  get max_turns() { return this.fields.max_turns; }
  get num_ctx() { return this.fields.num_ctx; }
  get num_predict() { return this.fields.num_predict; }
  get temperature() { return this.fields.temperature; }
  get seed() { return this.fields.seed; }
  get request_timeout_seconds() { return this.fields.request_timeout_seconds; }
  get retry_attempts() { return this.fields.retry_attempts; }
  get retry_backoff_seconds() { return this.fields.retry_backoff_seconds; }
  get tool_timeout_seconds() { return this.fields.tool_timeout_seconds; }
  get keep_alive() { return this.fields.keep_alive; }
  get think() { return this.fields.think; }
  get max_parse_repairs() { return this.fields.max_parse_repairs; }
  get parse_repair_echo_chars() { return this.fields.parse_repair_echo_chars; }
  get context() { return this.fields.context; }

  static fromEnv(options: { defaultToolTimeoutSeconds: number; env?: Env }): AgentRuntimeConfig {
    const env = options.env ?? process.env;
    const context = ContextPolicy.fromEnv(env);
    const config = new AgentRuntimeConfig({
      max_turns: envInt(env, "FREELLAMA_AGENT_MAX_TURNS", 10, 1),
      num_ctx: envInt(env, "FREELLAMA_AGENT_NUM_CTX", 8192, 1),
      num_predict: envInt(env, "FREELLAMA_AGENT_NUM_PREDICT", 512, 1),
      temperature: envFloat(env, "FREELLAMA_AGENT_TEMPERATURE", 0),
      seed: envInt(env, "FREELLAMA_AGENT_SEED", 42),
      request_timeout_seconds: envFloat(env, "FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS", 600, 0.001),
      retry_attempts: envInt(env, "FREELLAMA_AGENT_RETRY_ATTEMPTS", 2, 1),
      retry_backoff_seconds: envFloat(env, "FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS", 5),
      tool_timeout_seconds: envFloat(env, "FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS", options.defaultToolTimeoutSeconds, 0.001),
      keep_alive: (env.FREELLAMA_AGENT_KEEP_ALIVE ?? "5m").trim(),
      think: envBool(env, "FREELLAMA_AGENT_THINK", false),
      max_parse_repairs: envInt(env, "FREELLAMA_AGENT_MAX_PARSE_REPAIRS", MAX_PARSE_REPAIRS, 0),
      parse_repair_echo_chars: envInt(env, "FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS", 500, 1),
      context,
    });
    if (contextBudget(config.num_ctx, config.num_predict, context.safety_margin_tokens) <= 0) {
      throw new ValidationError("FREELLAMA_AGENT_NUM_CTX must exceed FREELLAMA_AGENT_NUM_PREDICT plus FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS");
    }
    if (!config.keep_alive) throw new ValidationError("FREELLAMA_AGENT_KEEP_ALIVE must not be empty");
    return config;
  }

  metadata(): Record<string, unknown> {
    const { context: _context, ...rest } = this.fields;
    return { ...rest };
  }
}
