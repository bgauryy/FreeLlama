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

/** GET the model catalog (`/api/tags`, or serve's `/_freellama/v1/models`) as bounded JSON. */
export async function readCatalogJson(url: string, request: typeof httpRequest = httpRequest): Promise<unknown> {
  const payload = await request(url, { method: "GET", headers: requestHeaders(), timeoutSeconds: 5, limit: IDENTITY_LIMIT + 1 });
  if (payload.length > IDENTITY_LIMIT) throw new ValidationError("model identity catalog exceeds size limit");
  return parseJsonBody(payload, "model identity catalog");
}

/**
 * The catalog entry named `model` (or `model:latest` for an untagged name). `ambiguous` when an
 * entry's name is a list or object, `null` when the payload is not a catalog or lacks the model.
 */
function catalogEntry(payload: unknown, model: string): Record<string, unknown> | null | "ambiguous" {
  const entries = isRecord(payload) ? payload.models ?? [] : [];
  if (!Array.isArray(entries)) return null;
  const names = new Set([model]);
  const lastSegment = model.slice(model.lastIndexOf("/") + 1);
  if (!lastSegment.includes(":")) names.add(model + ":latest");
  for (const item of entries) {
    if (!isRecord(item)) continue;
    const name = Object.hasOwn(item, "name") ? item.name : item.model;
    if (typeof name === "object" && name !== null) return "ambiguous";
    if (typeof name === "string" && names.has(name)) return item;
  }
  return null;
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
    const entry = catalogEntry(await (options.readJson ?? readCatalogJson)(url), model);
    // A list or object name makes the catalog ambiguous: identity stays unverified.
    if (entry === "ambiguous") return unknown;
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

/** The prompt-token ratio serve learned for a model from its own managed chats. */
export interface PlatformTokenRatio {
  /** UTF-8 bytes of the serialized chat messages per prompt token Ollama counted, times 1000. */
  bytes_per_token_milli: number;
  bytes_per_token_samples: number;
}

/**
 * Read the chosen model's learned bytes-per-token ratio from serve's catalog, once per run. Returns
 * `null` when the ratio is unmeasured, the model is absent, or serve is unreachable, older (no
 * field) or refuses; the caller then keeps its local estimate.
 */
export async function resolvePlatformTokenRatio(
  serveEndpoint: string,
  model: string,
  options: { readJson?: (url: string) => unknown | Promise<unknown> } = {},
): Promise<PlatformTokenRatio | null> {
  try {
    const url = `${trimSlashes(serveEndpoint)}/_freellama/v1/models`;
    const entry = catalogEntry(await (options.readJson ?? readCatalogJson)(url), model);
    if (entry === null || entry === "ambiguous") return null;
    const milli = entry.bytes_per_token_milli;
    if (typeof milli !== "number" || !Number.isSafeInteger(milli) || milli <= 0) return null;
    const samples = entry.bytes_per_token_samples;
    return {
      bytes_per_token_milli: milli,
      bytes_per_token_samples: typeof samples === "number" && Number.isSafeInteger(samples) && samples > 0 ? samples : 1,
    };
  } catch {
    // Optional evidence: any failure to read it falls back to the local estimate.
    return null;
  }
}

/** A chat request failure the adapter loop reports instead of crashing. */
export function isTransportError(error: unknown): error is HTTPError | URLError | RequestTimeoutError {
  return error instanceof HTTPError || error instanceof URLError || error instanceof RequestTimeoutError;
}

/** Retry known refusals only; a timeout may leave inference running upstream. */
export function retryableChatError(error: unknown): boolean {
  if (error instanceof HTTPError) {
    if (error.code !== 503 || error.body.length > RECEIPT_BODY_LIMIT) return false;
    try {
      const body = parseJsonBody(error.body, "retry receipt");
      if (!isRecord(body) || body.retry_safe === false
        || (body.execution_outcome !== undefined && body.execution_outcome !== "not_started")) return false;
      return (body.retry_safe === true && body.execution_outcome === "not_started")
        || ["admission_timeout", "resource_admission_unavailable", "upstream_circuit_open", "model_lifecycle_busy"].includes(String(body.code));
    } catch { return false; }
  }
  return error instanceof URLError && (error.reason as NodeJS.ErrnoException).code === "ECONNREFUSED";
}

const RECEIPT_FIELDS = new Set([
  "error", "code", "reason", "retry_after_seconds", "resource_admission", "lifecycle",
  "placement", "execution", "admission", "route", "decision", "quality", "feedback",
  "execution_outcome", "retry_safe", "result_recoverable", "job_id", "last_observed_status",
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
