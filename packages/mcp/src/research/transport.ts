// Ollama/FreeLlama chat protocol for the research adapters: model identity resolution, direct and
// managed request bodies, refusal receipts, retry classification and prompt-cache accounting. The
// HTTP client lives in transport-http.ts.
import { createHash } from "node:crypto";
import { canonicalJson, isRecord } from "./canonical-json.js";
import { ValidationError, isSystemError } from "./errors.js";
import {
  HTTPError, RECEIPT_BODY_LIMIT, RequestTimeoutError, URLError, httpRequest, parseJsonBody, requestHeaders,
} from "./transport-http.js";

export {
  HTTPError, RequestTimeoutError, URLError, httpRequest, requestHeaders, requestJson,
} from "./transport-http.js";

const MANAGED_SUFFIX = "/_freellama/v1/tasks";
const IDENTITY_LIMIT = 1024 * 1024;

const trimSlashes = (text: string) => text.replace(/\/+$/, "");

async function readIdentityJson(url: string): Promise<unknown> {
  const payload = await httpRequest(url, { method: "GET", headers: requestHeaders(), timeoutSeconds: 5, limit: IDENTITY_LIMIT + 1 });
  if (payload.length > IDENTITY_LIMIT) throw new ValidationError("model identity catalog exceeds size limit");
  return parseJsonBody(payload, "model identity catalog");
}

export interface ModelIdentity {
  identity: string;
  verified: boolean;
  scope: string;
  digest?: string;
}

const DIGEST = /^(?:sha256:)?[0-9a-fA-F]{64}$/;
const sha256 = (data: string) => createHash("sha256").update(data, "utf8").digest("hex");

/**
 * Resolve immutable manifest identity once per adapter run, without loading a model. Managed
 * catalogs honor exact CPU backend assignment. Failure disables persistent calibration; a mutable
 * tag or a hash of the prompt alone never masquerades as model identity.
 */
export async function resolveModelIdentity(
  endpointInput: string,
  model: string,
  systemPrompt: string,
  options: { readJson?: (url: string) => unknown | Promise<unknown> } = {},
): Promise<ModelIdentity> {
  const endpoint = trimSlashes(endpointInput);
  const url = endpoint.endsWith(MANAGED_SUFFIX)
    ? endpoint.slice(0, -MANAGED_SUFFIX.length) + "/_freellama/v1/models"
    : endpoint + "/api/tags";
  const unknown: ModelIdentity = { identity: "", verified: false, scope: "current_process_only" };
  try {
    const payload = await (options.readJson ?? readIdentityJson)(url);
    const entries = isRecord(payload) ? payload.models ?? [] : [];
    if (!Array.isArray(entries)) return unknown;
    const names = new Set([model]);
    const lastSegment = model.slice(model.lastIndexOf("/") + 1);
    if (!lastSegment.includes(":")) names.add(model + ":latest");
    let entry: Record<string, unknown> | null = null;
    for (const item of entries) {
      if (!isRecord(item)) continue;
      const name = Object.hasOwn(item, "name") ? item.name : item.model;
      // A list or object name makes the catalog ambiguous: identity stays unverified.
      if (typeof name === "object" && name !== null) return unknown;
      if (typeof name === "string" && names.has(name)) {
        entry = item;
        break;
      }
    }
    const digestValue = entry?.digest;
    if (typeof digestValue !== "string" || !DIGEST.test(digestValue)) return unknown;
    const digest = (digestValue.startsWith("sha256:") ? digestValue.slice("sha256:".length) : digestValue).toLowerCase();
    const contract = { endpoint, model, digest, system_hash: sha256(systemPrompt) };
    return {
      identity: sha256(canonicalJson(contract)),
      verified: true,
      scope: "manifest_digest_and_system_prompt",
      digest,
    };
  } catch (error) {
    if (isTransportError(error) || isSystemError(error) || error instanceof ValidationError
      || error instanceof SyntaxError || error instanceof TypeError) {
      return unknown;
    }
    throw error;
  }
}

/** A chat request failure the adapter loop reports instead of crashing. */
export function isTransportError(error: unknown): error is HTTPError | URLError | RequestTimeoutError {
  return error instanceof HTTPError || error instanceof URLError || error instanceof RequestTimeoutError;
}

/** Retry known refusals only; a timeout may leave inference running upstream. */
export function retryableChatError(error: unknown): boolean {
  if (error instanceof HTTPError) return error.code === 503;
  return error instanceof URLError && (error.reason as NodeJS.ErrnoException).code === "ECONNREFUSED";
}

const RECEIPT_FIELDS = new Set([
  "error", "code", "reason", "retry_after_seconds", "resource_admission", "lifecycle",
  "placement", "execution", "admission", "route", "decision", "quality", "feedback",
]);
const PAYLOAD_FIELDS = new Set([
  "prompt", "system", "system_prompt", "messages", "content", "input", "images", "tools",
  "request", "request_options", "headers", "authorization", "token", "api_key", "auth_token",
]);

function receiptValue(value: unknown): unknown {
  if (isRecord(value)) {
    const result: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value)) {
      if (!PAYLOAD_FIELDS.has(key.toLowerCase())) result[key] = receiptValue(item);
    }
    return result;
  }
  if (Array.isArray(value)) return value.map(receiptValue);
  return value;
}

/** `<error name>: <message>` for an adapter-visible error. */
export function errorDiagnostic(error: unknown): string {
  if (error instanceof Error) return `${error.name}: ${error.message}`;
  return String(error);
}

/** Retain bounded application refusal receipts, excluding request/auth payloads. */
export function chatErrorDetails(error: unknown): [string, Record<string, unknown> | null] {
  let diagnostic = errorDiagnostic(error);
  if (!(error instanceof HTTPError)) return [diagnostic, null];
  const details: Record<string, unknown> = { status: error.code };
  try {
    const body = error.body.subarray(0, RECEIPT_BODY_LIMIT + 1);
    if (body.length > RECEIPT_BODY_LIMIT) return [diagnostic, details];
    const payload = parseJsonBody(body, "refusal body");
    if (!isRecord(payload) || typeof payload.error !== "string") return [diagnostic, details];
    const receipt: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(payload)) {
      if (RECEIPT_FIELDS.has(key)) receipt[key] = receiptValue(value);
    }
    details.receipt = receipt;
    diagnostic = `HTTP ${error.code}: ${receipt.error as string}`;
  } catch (failure) {
    if (!(failure instanceof SyntaxError || failure instanceof ValidationError || failure instanceof RangeError)) throw failure;
  }
  return [diagnostic, details];
}

/** Aggregate optional Ollama cache reads without treating unavailable turns as zero. */
export class PromptCacheUsage {
  calls = 0;
  reportedCalls = 0;
  reportedTokens = 0;
  reportedPromptTokens = 0;

  observe(response: Record<string, unknown>): void {
    this.calls += 1;
    const cached = response.prompt_eval_cached_count;
    const prompt = response.prompt_eval_count;
    if (Number.isInteger(cached) && Number.isInteger(prompt) && (cached as number) >= 0 && (cached as number) <= (prompt as number)) {
      this.reportedCalls += 1;
      this.reportedTokens += cached as number;
      this.reportedPromptTokens += prompt as number;
    }
  }

  get tokens(): number | null {
    return this.calls > 0 && this.calls === this.reportedCalls ? this.reportedTokens : null;
  }

  metadata(): Record<string, unknown> {
    const tokens = this.tokens;
    return {
      status: tokens !== null ? "reported" : this.reportedCalls ? "partially_reported" : "not_reported",
      calls: this.calls,
      reported_calls: this.reportedCalls,
      reported_tokens: this.reportedCalls ? this.reportedTokens : null,
      hit_ratio: tokens !== null && this.reportedPromptTokens ? this.reportedTokens / this.reportedPromptTokens : null,
    };
  }
}

/**
 * Build one direct-Ollama or managed-FreeLlama request. A URL ending in `/_freellama/v1/tasks` is
 * managed: coding-agent turns then share routing, admission, physical-placement receipts, and
 * adaptive feedback with ordinary `run_task` calls. Raw Ollama stays available for controlled
 * benchmark comparisons.
 */
export function chatRequest(
  endpointInput: string,
  model: string,
  messages: Record<string, unknown>[],
  options: Record<string, unknown>,
  think: boolean,
  keepAlive: string,
  executionPreference = "auto",
  minPlacementEvidence = "configured",
): [string, Record<string, unknown>] {
  const endpoint = trimSlashes(endpointInput);
  if (endpoint.endsWith(MANAGED_SUFFIX)) {
    const managedOptions: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(options)) {
      if (key !== "num_ctx") managedOptions[key] = value;
    }
    if (!Object.hasOwn(options, "num_ctx")) throw new ValidationError("managed chat request requires num_ctx");
    return [endpoint, {
      task: "coding",
      objective: "fastest",
      model,
      context_tokens: options.num_ctx,
      execution_preference: executionPreference,
      min_placement_evidence: minPlacementEvidence,
      messages,
      keep_alive: keepAlive,
      request_options: { format: "json", think, options: managedOptions },
    }];
  }
  return [`${endpoint}/api/chat`, {
    model,
    messages,
    stream: false,
    truncate: false,
    shift: false,
    format: "json",
    think,
    keep_alive: keepAlive,
    options,
  }];
}

/** Return the Ollama response and retain managed execution proof when present. */
export function unwrapChatResponse(payload: unknown, executionReceipts: Record<string, unknown>[]): Record<string, unknown> {
  if (!isRecord(payload)) throw new TypeError("chat response must be a JSON object");
  const { response } = payload;
  if (!isRecord(response)) return payload;
  const route = payload.route ?? {};
  if (!isRecord(route)) throw new TypeError("managed chat response route must be a JSON object");
  executionReceipts.push({
    model: route.selected_model ?? null,
    execution: payload.execution ?? null,
    admission: payload.admission ?? null,
    feedback: payload.feedback ?? null,
  });
  return response;
}
