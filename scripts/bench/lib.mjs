// Common generation measurements. Vendor scheduling deliberately uses awaited loops.
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { appendFile, mkdir, open, readFile, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { sampleHost, startHostSampler, summarizeHost } from "./host.mjs";
import { hogDecision, parseHogGb, startMemoryHog } from "./memory-hog.mjs";
import { renderDashboard } from "./report.mjs";
import { DEFAULT_OLLAMA_BASE_URL, OLLAMA_BACKED, evaluateLoadGate, loadSnapshot, ollamaResidency, sampleProcessRss, unloadOllamaModel } from "./resources.mjs";

export const CASE = Object.freeze({
  id: "extract-ticket-v1",
  messages: [
    { role: "system", content: "Extract the ticket fields from the supplied text. Return only a JSON object with exactly the keys id, team, and priority. Do not add Markdown, explanation, or other fields." },
    { role: "user", content: "Ticket id: FL-104. Team: billing. Priority: high. The customer reports a duplicate charge." },
  ],
  expected: { id: "FL-104", team: "billing", priority: "high" },
});

const delay = (ms, signal) => new Promise((resolve, reject) => {
  if (signal?.aborted) return reject(signal.reason);
  const timer = setTimeout(done, ms);
  function done() { signal?.removeEventListener("abort", abort); resolve(); }
  function abort() { clearTimeout(timer); reject(signal.reason); }
  signal?.addEventListener("abort", abort, { once: true });
});
const sha256 = (value) => createHash("sha256").update(value).digest("hex");
const workspaceRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
export const DEFAULT_LOCK_PATH = path.join(workspaceRoot, ".octocode/benchmark.lock");
export const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;

function supportedFields(value, allowed, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${label} must be an object.`);
  for (const key of Object.keys(value)) if (!allowed.includes(key)) throw new Error(`Unknown ${label} field: ${key}`);
}
function validateDigest(value, label) {
  if (value !== undefined && (typeof value !== "string" || !/^(?:sha256:)?[a-f0-9]{64}$/i.test(value))) throw new Error(`${label} must be a SHA-256 digest.`);
}
const digestHex = value => typeof value === "string" ? value.replace(/^sha256:/i, "").toLowerCase() : null;

// Bound broken or unexpectedly verbose upstreams, including health and error bodies.
// Cancelling a client reader is not proof that server-side inference has stopped.
export async function readBoundedBody(response, { signal, maxBytes = MAX_RESPONSE_BYTES, onText } = {}) {
  if (!response.body) return "";
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const chunks = [];
  let bytesRead = 0;
  let ended = false;
  const emit = (text) => { if (onText) onText(text); else chunks.push(text); };
  const cancel = () => { void reader.cancel(signal?.reason).catch(() => {}); };
  signal?.addEventListener("abort", cancel, { once: true });
  try {
    signal?.throwIfAborted();
    for (;;) {
      const { done, value } = await reader.read();
      signal?.throwIfAborted();
      if (done) { ended = true; break; }
      const allowed = Math.max(0, maxBytes - bytesRead);
      if (value.byteLength > allowed) {
        if (allowed) emit(decoder.decode(value.subarray(0, allowed), { stream: true }));
        throw Object.assign(new Error(`Response exceeded ${maxBytes} bytes; capture truncated and remaining inference is uncertain.`), { executionUncertain: true, captureTruncated: true });
      }
      bytesRead += value.byteLength;
      emit(decoder.decode(value, { stream: true }));
    }
    emit(decoder.decode());
    return chunks.join("");
  } finally {
    signal?.removeEventListener("abort", cancel);
    if (!ended) cancel();
    reader.releaseLock();
  }
}

export function validateConfig(config, { platform = process.platform } = {}) {
  if (!config || typeof config !== "object" || !config.model || !Array.isArray(config.vendors)) throw new Error("Config requires model and vendors.");
  supportedFields(config, ["model", "vendors", "ollama_base_url"], "config");
  if (config.ollama_base_url !== undefined) {
    const url = new URL(config.ollama_base_url);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== "/") throw new Error("ollama_base_url must be an http(s) origin without credentials or path.");
  }
  supportedFields(config.model, ["ollama_tag", "artifact_sha256", "quantization", "context_tokens", "output_tokens"], "model");
  validateDigest(config.model.artifact_sha256, "model.artifact_sha256");
  for (const key of ["context_tokens", "output_tokens"]) {
    const value = config.model[key] ?? (key === "context_tokens" ? 4096 : 64);
    if (!Number.isSafeInteger(value) || value < 1) throw new Error(`model.${key} must be a positive integer.`);
  }
  const ids = new Set();
  for (const vendor of config.vendors) {
    if (vendor.stop !== undefined) throw new Error(`Do not configure stop for ${vendor.id}; only a process started by this runner is stopped.`);
    supportedFields(vendor, ["id", "protocol", "base_url", "model", "enabled", "version", "timeout_ms", "start", "cohort", "artifact_sha256"], "vendor");
    if (!/^[a-z0-9][a-z0-9_-]*$/.test(vendor.id ?? "") || ids.has(vendor.id)) throw new Error("Each vendor needs a unique, filename-safe id.");
    ids.add(vendor.id);
    if (vendor.enabled !== undefined && typeof vendor.enabled !== "boolean") throw new Error(`enabled for ${vendor.id} must be a boolean.`);
    if (vendor.cohort !== undefined && (typeof vendor.cohort !== "string" || !vendor.cohort.trim())) throw new Error(`cohort for ${vendor.id} must be a nonempty string.`);
    validateDigest(vendor.artifact_sha256, `artifact_sha256 for ${vendor.id}`);
    if (!["ollama", "openai", "freellama", "freellama-managed", "freellama-stream"].includes(vendor.protocol)) throw new Error(`Unsupported protocol for ${vendor.id}.`);
    const url = new URL(vendor.base_url);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) throw new Error(`Invalid base_url for ${vendor.id}; do not put credentials in URLs.`);
    if (url.pathname !== "/") throw new Error(`base_url for ${vendor.id} must be an origin, without /v1 or other path.`);
    if (!vendor.model && !config.model.ollama_tag) throw new Error(`No model configured for ${vendor.id}.`);
    if (vendor.timeout_ms !== undefined && (!Number.isSafeInteger(vendor.timeout_ms) || vendor.timeout_ms < 1)) throw new Error(`Invalid timeout_ms for ${vendor.id}.`);
    if (vendor.start) {
      supportedFields(vendor.start, ["command", "args", "cwd", "env", "ready_timeout_ms"], "start");
      if (platform === "win32" && vendor.enabled !== false) throw new Error("Owned service startup requires POSIX process-group cleanup; start Windows services externally.");
      if (typeof vendor.start.command !== "string" || !vendor.start.command || !Array.isArray(vendor.start.args) || vendor.start.args.some((arg) => typeof arg !== "string")) throw new Error(`start for ${vendor.id} needs command and an args array; shell commands are not supported.`);
      if (vendor.start.cwd !== undefined && typeof vendor.start.cwd !== "string") throw new Error(`start.cwd for ${vendor.id} must be a string.`);
      if (vendor.start.env && (typeof vendor.start.env !== "object" || Array.isArray(vendor.start.env) || Object.values(vendor.start.env).some((value) => typeof value !== "string"))) throw new Error(`start.env for ${vendor.id} must contain string values.`);
      if (vendor.start.ready_timeout_ms !== undefined && (!Number.isSafeInteger(vendor.start.ready_timeout_ms) || vendor.start.ready_timeout_ms < 1 || vendor.start.ready_timeout_ms > 300_000)) throw new Error(`start.ready_timeout_ms for ${vendor.id} must be an integer from 1 to 300000.`);
    }
  }
  return config;
}

export function requestFor(vendor, modelConfig = {}) {
  const model = vendor.model ?? modelConfig.ollama_tag;
  const outputTokens = modelConfig.output_tokens ?? 64;
  const base = vendor.base_url.replace(/\/$/, "");
  if (vendor.protocol === "openai") return {
    url: `${base}/v1/chat/completions`,
    body: { model, messages: CASE.messages, temperature: 0, seed: 42, max_tokens: outputTokens, stream: true, stream_options: { include_usage: true } },
  };
  if (vendor.protocol === "freellama-managed" || vendor.protocol === "freellama-stream") return {
    url: `${base}/_freellama/v1/tasks`,
    body: { task: "completion", model, messages: CASE.messages, context_tokens: modelConfig.context_tokens ?? 4096, request_options: { think: false, options: { temperature: 0, seed: 42, num_predict: outputTokens } }, ...(vendor.protocol === "freellama-stream" ? { stream: true } : {}) },
  };
  return {
    url: `${base}/api/chat`,
    body: { model, messages: CASE.messages, think: false, stream: true, options: { temperature: 0, seed: 42, num_ctx: modelConfig.context_tokens ?? 4096, num_predict: outputTokens } },
  };
}

export function grade(text) {
  try {
    const value = JSON.parse(text.trim());
    const keys = value && typeof value === "object" && !Array.isArray(value) ? Object.keys(value).sort() : [];
    const schemaValid = keys.join(",") === "id,priority,team" && keys.every((key) => typeof value[key] === "string");
    return { json_valid: true, schema_valid: schemaValid, passed: schemaValid && Object.entries(CASE.expected).every(([key, expected]) => value[key] === expected) };
  } catch { return { json_valid: false, schema_valid: false, passed: false }; }
}

export function distribution(values) {
  const ordered = values.filter((value) => typeof value === "number" && Number.isFinite(value)).sort((a, b) => a - b);
  if (!ordered.length) return { n: 0, p50: null, p95: null, p99: null, min: null, max: null, spread: null };
  const at = (fraction) => ordered[Math.max(0, Math.ceil(ordered.length * fraction) - 1)];
  return { n: ordered.length, p50: at(0.5), p95: at(0.95), p99: at(0.99), min: ordered[0], max: ordered.at(-1), spread: ordered.at(-1) - ordered[0] };
}

const finiteNonnegative = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
const durationMs = (value) => finiteNonnegative(value) === null ? null : value / 1e6;
const refusalCodes = new Set(["resource_admission_unavailable", "admission_queue_full", "admission_timeout", "job_no_model_fits"]);

// Managed /tasks returns a nonstreaming receipt. Its server timestamps cannot establish client TTFT.
function readManagedReceipt(result, receipt, response, requestedModel) {
  if (!receipt || typeof receipt !== "object" || Array.isArray(receipt)) throw new Error("Managed response is not a receipt object.");
  result.receipt = receipt;
  for (const key of ["route", "admission", "execution"]) result[key] = receipt[key] ?? null;
  result.server_timing = receipt.timing ?? null;
  const metrics = receipt.metrics ?? {};
  result.metrics = {
    output_tokens: finiteNonnegative(metrics.output_tokens),
    prompt_tokens: finiteNonnegative(metrics.prompt_tokens),
    load_ms: durationMs(metrics.load_duration_ns),
    prompt_eval_ms: durationMs(metrics.prompt_duration_ns),
    output_tokens_per_second: finiteNonnegative(metrics.output_tokens_per_second),
  };
  const code = receipt.code ?? receipt.error?.code;
  const executionOutcome = receipt.execution_outcome ?? receipt.error?.execution_outcome;
  const retrySafe = receipt.retry_safe ?? receipt.error?.retry_safe;
  if (["unknown", "started"].includes(executionOutcome)) result.execution_uncertain = true;
  if (receipt.response?.done === true && typeof receipt.response.message?.content === "string") result.server_terminal = true;
  if (!response.ok || receipt.error || code || receipt.response?.error || executionOutcome !== undefined && executionOutcome !== "completed") {
    // A status or code never turns an uncertain/post-start execution into a safe refusal.
    const preExecution = retrySafe !== false && (executionOutcome === "not_started" || executionOutcome === undefined);
    result.outcome = preExecution && (response.status === 429 || refusalCodes.has(code)) ? "refused" : "failed";
    result.pre_execution_refusal = result.outcome === "refused";
    throw new Error(`Managed task ${response.status}${code ? `: ${code}` : ""}${executionOutcome ? ` (${executionOutcome})` : ""}: ${typeof receipt.error === "string" ? receipt.error : receipt.response?.error ?? "request did not complete"}`);
  }
  if (typeof receipt.route?.selected_model !== "string") throw new Error("Managed receipt has no selected-model attribution for the pinned benchmark model.");
  if (receipt.route.selected_model !== requestedModel) throw new Error("Managed receipt names a different model than the pinned benchmark model.");
  if (receipt.response?.model !== undefined && receipt.response.model !== requestedModel) throw new Error("Managed response names a different model than the pinned benchmark model.");
  if (receipt.response?.done !== true || typeof receipt.response.message?.content !== "string") throw new Error("Managed receipt has no completed chat response.");
  result.content = receipt.response.message.content;
  result.thinking = typeof receipt.response.message.thinking === "string" ? receipt.response.message.thinking : "";
  result.response_complete = true;
  result.grade = grade(result.content);
  result.outcome = result.grade.passed ? "verified" : "quality_failed";
}

// freellama-stream: SSE events admission, route, delta, then exactly one terminal done or error.
// Client TTFT is the first delta with nonempty content. done carries the nonstreaming receipt,
// which is graded exactly like freellama-managed; error carries the error body plus retry_safe.
async function readManagedStream(result, response, signal, requestedModel, now, started) {
  const stream = result.stream = { events: {}, admission: [], route: null, streamed_content: "", streamed_thinking: "", streamed_content_matches_done: null, terminal_event: null };
  let pending = "", event = "", data = [], terminal = null, terminalError = null;
  const dispatch = () => {
    if (!data.length) { event = ""; return; }
    const name = event || "message", text = data.join("\n");
    event = ""; data = [];
    stream.events[name] = (stream.events[name] ?? 0) + 1;
    if (terminal) { terminalError ??= "Stream data arrived after its terminal marker."; return; }
    let payload;
    try { payload = JSON.parse(text); } catch (error) { terminalError ??= `Malformed ${name} event: ${error.message}`; return; }
    if (name === "admission") stream.admission.push(payload);
    else if (name === "route") stream.route = payload;
    else if (name === "delta") {
      if (typeof payload?.content === "string" && payload.content.length) {
        if (result.first_content_ms === null) result.first_content_ms = now() - started;
        stream.streamed_content += payload.content;
      }
      if (typeof payload?.thinking === "string") stream.streamed_thinking += payload.thinking;
    } else if (name === "done" || name === "error") terminal = { name, payload };
  };
  const line = (value) => {
    if (value === "") return dispatch();
    if (value.startsWith(":")) return;
    const colon = value.indexOf(":");
    const field = colon === -1 ? value : value.slice(0, colon);
    const fieldValue = colon === -1 ? "" : value.slice(colon + 1).replace(/^ /, "");
    if (field === "event") event = fieldValue;
    else if (field === "data") data.push(fieldValue);
  };
  await readBoundedBody(response, { signal, onText: decoded => {
    result.raw_response += decoded;
    pending += decoded;
    let newline;
    while ((newline = pending.indexOf("\n")) !== -1) { line(pending.slice(0, newline).replace(/\r$/, "")); pending = pending.slice(newline + 1); }
  } });
  if (pending) line(pending.replace(/\r$/, ""));
  dispatch();
  if (terminalError) throw new Error(terminalError);
  if (!terminal) throw new Error("Truncated stream: no done or error event was received.");
  stream.terminal_event = terminal.name;
  if (terminal.name === "error") readManagedReceipt(result, terminal.payload, { ok: false, status: response.status }, requestedModel);
  readManagedReceipt(result, terminal.payload, response, requestedModel);
  result.completed_stream = true;
  stream.streamed_content_matches_done = stream.streamed_content === result.content;
}

// Preserve the wire representation even when a later frame is malformed or EOF is premature.
export async function measureRequest(vendor, modelConfig, { fetchImpl = fetch, signal, now = () => performance.now() } = {}) {
  const request = requestFor(vendor, modelConfig);
  const started = now();
  const streamed = vendor.protocol === "freellama-stream";
  const managed = vendor.protocol === "freellama-managed";
  const result = { request, http_status: null, response_headers: null, raw_response: "", content: "", thinking: "", first_content_ms: null, elapsed_ms: null, completed_stream: managed ? null : false, response_complete: false, execution_uncertain: false, pre_execution_refusal: false, receipt: null, route: null, admission: null, execution: null, server_timing: null, metrics: { output_tokens: null, prompt_tokens: null, load_ms: null, prompt_eval_ms: null, output_tokens_per_second: null }, outcome: "failed", error: null, grade: null };
  let terminalError = null;
  result.client_start_ms = started;
  result.client_end_ms = null;
  let submitted = false;
  try {
    const requestSignal = AbortSignal.any([AbortSignal.timeout(vendor.timeout_ms ?? 120_000), ...(signal ? [signal] : [])]);
    requestSignal.throwIfAborted();
    submitted = true;
    const response = await fetchImpl(request.url, { method: "POST", redirect: "error", headers: { "content-type": "application/json" }, body: JSON.stringify(request.body), signal: requestSignal });
    result.http_status = response.status;
    result.response_headers = Object.fromEntries([...response.headers].filter(([key]) => ["content-type", "retry-after", "server"].includes(key)));
    if (streamed && response.ok && /text\/event-stream/i.test(response.headers.get("content-type") ?? "")) {
      await readManagedStream(result, response, requestSignal, request.body.model, now, started);
      result.client_end_ms = now();
      result.elapsed_ms = result.client_end_ms - started;
      return result;
    }
    // A JSON body (for example a refusal before streaming starts) is read as a managed receipt.
    if (managed || streamed) {
      if (streamed) result.stream = { events: {}, non_sse_response: true };
      await readBoundedBody(response, { signal: requestSignal, onText: text => { result.raw_response += text; } });
      let receipt;
      try { receipt = JSON.parse(result.raw_response); } catch { throw new Error(`Managed task HTTP ${response.status}: response is not valid JSON.`); }
      readManagedReceipt(result, receipt, response, request.body.model);
      result.client_end_ms = now();
      result.elapsed_ms = result.client_end_ms - started;
      return result;
    }
    if (!response.ok) {
      await readBoundedBody(response, { signal: requestSignal, onText: text => { result.raw_response += text; } });
      let code, payload; try { payload = JSON.parse(result.raw_response); code = payload?.code; } catch { /* Preserve the original body. */ }
      const executionOutcome = payload?.execution_outcome ?? payload?.error?.execution_outcome;
      const retrySafe = payload?.retry_safe ?? payload?.error?.retry_safe;
      const preExecution = retrySafe !== false && (executionOutcome === undefined || executionOutcome === "not_started");
      result.outcome = preExecution && (response.status === 429 || refusalCodes.has(code)) ? "refused" : "failed";
      result.pre_execution_refusal = result.outcome === "refused";
      throw new Error(`HTTP ${response.status}${code ? `: ${code}` : ""}`);
    }
    if (!response.body) throw new Error("Successful response has no body.");
    const consume = (data) => {
      if (result.completed_stream) { terminalError ??= "Stream data arrived after its terminal marker."; return; }
      if (vendor.protocol === "openai" && data === "[DONE]") { result.completed_stream = true; return; }
      let frame;
      try { frame = JSON.parse(data); } catch (error) { terminalError ??= `Malformed stream frame: ${error.message}`; return; }
      if (!frame || typeof frame !== "object" || Array.isArray(frame)) { terminalError ??= "Stream frame is not an object."; return; }
      if (frame.error) { terminalError = typeof frame.error === "string" ? frame.error : JSON.stringify(frame.error); return; }
      if (vendor.protocol !== "openai" && frame.model !== undefined && frame.model !== request.body.model) terminalError ??= "Native response names a different model than the pinned benchmark model.";
      const content = vendor.protocol === "openai" ? frame.choices?.[0]?.delta?.content : frame.message?.content;
      const thinking = vendor.protocol === "openai" ? frame.choices?.[0]?.delta?.reasoning_content : frame.message?.thinking;
      if (typeof content === "string" && content.length) {
        if (result.first_content_ms === null) result.first_content_ms = now() - started;
        result.content += content;
      }
      if (typeof thinking === "string") result.thinking += thinking;
      if (vendor.protocol === "openai") {
        if (frame.usage && Object.hasOwn(frame.usage, "completion_tokens")) result.metrics.output_tokens = finiteNonnegative(frame.usage.completion_tokens);
        if (frame.usage && Object.hasOwn(frame.usage, "prompt_tokens")) result.metrics.prompt_tokens = finiteNonnegative(frame.usage.prompt_tokens);
      } else {
        if (frame.done === true) result.completed_stream = true;
        if (Object.hasOwn(frame, "eval_count")) result.metrics.output_tokens = finiteNonnegative(frame.eval_count);
        if (Object.hasOwn(frame, "prompt_eval_count")) result.metrics.prompt_tokens = finiteNonnegative(frame.prompt_eval_count);
        if (Object.hasOwn(frame, "load_duration")) result.metrics.load_ms = durationMs(frame.load_duration);
        if (Object.hasOwn(frame, "prompt_eval_duration")) result.metrics.prompt_eval_ms = durationMs(frame.prompt_eval_duration);
        if (Object.hasOwn(frame, "eval_count") || Object.hasOwn(frame, "eval_duration")) {
          const count = finiteNonnegative(frame.eval_count), duration = finiteNonnegative(frame.eval_duration);
          result.metrics.output_tokens_per_second = count !== null && duration > 0 ? finiteNonnegative(count / (duration / 1e9)) : null;
        }
      }
    };
    let pending = "";
    let sseData = [];
    const line = (value) => {
      if (vendor.protocol !== "openai") { if (value.trim()) consume(value); return; }
      if (value === "") {
        if (sseData.length) { consume(sseData.join("\n")); sseData = []; }
      } else if (value.startsWith("data:")) sseData.push(value.slice(5).replace(/^ /, ""));
    };
    await readBoundedBody(response, { signal: requestSignal, onText: decoded => {
      result.raw_response += decoded;
      pending += decoded;
      let newline;
      while ((newline = pending.indexOf("\n")) !== -1) { line(pending.slice(0, newline).replace(/\r$/, "")); pending = pending.slice(newline + 1); }
    } });
    if (pending) line(pending.replace(/\r$/, ""));
    if (sseData.length) consume(sseData.join("\n"));
    if (terminalError) throw new Error(terminalError);
    if (!result.completed_stream) throw new Error("Truncated stream: terminal marker was not received.");
    result.response_complete = true;
    result.grade = grade(result.content);
    result.outcome = result.grade.passed ? "verified" : "quality_failed";
  } catch (error) {
    result.error = String(error.message ?? error);
    if (error.captureTruncated) result.raw_response_truncated = true;
    if (error.executionUncertain) result.execution_uncertain = true;
    if (submitted && !result.pre_execution_refusal && !result.response_complete && !result.server_terminal && !result.completed_stream) result.execution_uncertain = true;
  }
  result.client_end_ms = now();
  result.elapsed_ms = result.client_end_ms - started;
  return result;
}

async function getJson(url, fetchImpl, signal) {
  const requestSignal = AbortSignal.any([AbortSignal.timeout(5_000), ...(signal ? [signal] : [])]);
  const response = await fetchImpl(url, { redirect: "error", signal: requestSignal });
  const text = await readBoundedBody(response, { signal: requestSignal });
  if (!response.ok) throw Object.assign(new Error(`${url}: HTTP ${response.status}: ${text.slice(0, 500)}`), { observation: { url, http_status: response.status, raw_response: text } });
  try { return JSON.parse(text); } catch { throw Object.assign(new Error(`${url}: invalid JSON: ${text.slice(0, 500)}`), { observation: { url, http_status: response.status, raw_response: text } }); }
}

export async function health(vendor, modelConfig, { fetchImpl = fetch, signal } = {}) {
  const base = vendor.base_url.replace(/\/$/, "");
  const model = vendor.model ?? modelConfig.ollama_tag;
  if (vendor.protocol === "openai") {
    const models = await getJson(`${base}/v1/models`, fetchImpl, signal);
    if (!Array.isArray(models.data) || !models.data.some((entry) => entry.id === model)) throw Object.assign(new Error(`Requested model ${model} is absent from /v1/models.`), { observation: { models } });
    return { version: vendor.version ?? null, models };
  }
  const isFreeLlama = ["freellama", "freellama-managed", "freellama-stream"].includes(vendor.protocol);
  const ready = isFreeLlama ? await getJson(`${base}/_freellama/v1/ready`, fetchImpl, signal) : null;
  const version = await getJson(`${base}/api/version`, fetchImpl, signal);
  const models = await getJson(`${base}/api/tags`, fetchImpl, signal);
  if (!Array.isArray(models.models) || !models.models.some((entry) => [entry.name, entry.model].includes(model))) throw Object.assign(new Error(`Requested model ${model} is absent from /api/tags.`), { observation: { models, ready, version } });
  return { version: isFreeLlama ? vendor.version ?? null : version.version ?? null, upstream_version: version.version ?? null, ready, models };
}

export async function acquireLock(lockPath) {
  await mkdir(path.dirname(lockPath), { recursive: true });
  try { await mkdir(lockPath); } catch (error) {
    if (error.code === "EEXIST") throw new Error(`Another benchmark owns ${lockPath}. Check its owner.json; stale locks require manual review.`);
    throw error;
  }
  const token = randomUUID();
  try { await writeFile(path.join(lockPath, "owner.json"), `${JSON.stringify({ token, pid: process.pid, host: os.hostname(), started_at: new Date().toISOString() }, null, 2)}\n`); }
  catch (error) { await rm(lockPath, { recursive: true, force: true }); throw error; }
  let released = false;
  return async () => {
    if (released) return;
    let owner;
    try { owner = JSON.parse(await readFile(path.join(lockPath, "owner.json"), "utf8")); }
    catch (error) { if (error.code === "ENOENT") { released = true; return; } throw error; }
    if (owner.token !== token) throw new Error("Benchmark lock ownership changed; refusing to remove another run's lock.");
    await rm(lockPath, { recursive: true, force: true });
    released = true;
  };
}

async function ownedProcess(vendor, directory, { logName = "server.log" } = {}) {
  if (!vendor.start) return { child: null, stop: async () => {} };
  // The runner only targets a process group it created; existing vendor services are never stopped.
  const log = await open(path.join(directory, logName), "wx");
  let child;
  try { child = spawn(vendor.start.command, vendor.start.args, { cwd: vendor.start.cwd ?? process.cwd(), env: { ...process.env, ...vendor.start.env }, detached: true, stdio: ["ignore", log.fd, log.fd], shell: false }); }
  catch (error) { await log.close(); throw error; }
  let spawnError = null;
  child.once("error", (error) => { spawnError = error; });
  const stopped = new Promise((resolve) => child.once("close", resolve));
  return {
    child,
    assertRunning() { if (spawnError) throw spawnError; if (child.exitCode !== null || child.signalCode !== null) throw new Error(`Owned ${vendor.id} process exited before readiness.`); },
    async stop() {
      try { if (child.pid) {
        // Darwin answers EPERM, not ESRCH, while an owned group holds only an unreaped zombie;
        // the liveness probe below decides whether that group is really gone.
        const terminate = (signal) => { try { if (process.platform === "win32") child.kill(signal); else process.kill(-child.pid, signal); } catch (error) { if (error.code !== "ESRCH" && error.code !== "EPERM") throw error; } };
        terminate("SIGTERM");
        if (process.platform === "win32") {
          const finished = await Promise.race([stopped.then(() => true), delay(5_000).then(() => false)]);
          if (!finished) { terminate("SIGKILL"); await Promise.race([stopped, delay(2_000)]); }
        } else {
          const exited = () => child.exitCode !== null || child.signalCode !== null;
          const probe = () => { try { process.kill(-child.pid, 0); return "alive"; } catch (error) { if (error.code === "ESRCH" || error.code === "EPERM") return error.code; throw error; } };
          const alive = async () => {
            const state = probe();
            if (state !== "EPERM") return state === "alive";
            await Promise.race([stopped, delay(1_000)]);
            const again = probe();
            if (again === "EPERM" && !exited()) throw new Error(`Owned ${vendor.id} process group refused signals (EPERM) while its leader still runs.`);
            return again === "alive";
          };
          const deadline = Date.now() + 5_000;
          while (await alive() && Date.now() < deadline) await delay(50);
          if (await alive()) terminate("SIGKILL");
          const killDeadline = Date.now() + 2_000;
          while (await alive() && Date.now() < killDeadline) await delay(50);
          if (await alive()) throw new Error("Owned server process group did not exit; remaining vendors were not started.");
        }
      } } finally { await log.close(); }
    },
  };
}

export function summarizeVendor(vendor, trials, modelConfig = {}) {
  const measured = trials.filter((trial) => !trial.warmup);
  const verified = measured.filter((trial) => trial.outcome === "verified");
  const inheritsModel = vendor.cohort === undefined || vendor.cohort === "gguf";
  const result = { id: vendor.id, protocol: vendor.protocol, mode: vendor.protocol === "freellama-managed" ? "managed completion /_freellama/v1/tasks" : vendor.protocol === "freellama-stream" ? "managed streamed completion /_freellama/v1/tasks stream:true" : vendor.protocol === "freellama" ? "raw admission /api/chat" : "native generation API", comparison_cohort: vendor.cohort ?? null, declared_artifact_sha256: digestHex(vendor.artifact_sha256 ?? (inheritsModel ? modelConfig.artifact_sha256 : undefined)), quantization: inheritsModel ? modelConfig.quantization ?? null : null, model: vendor.model ?? modelConfig.ollama_tag ?? null, status: "complete", attempted: measured.length, success: verified.length, refused: 0, failed: 0, quality_failed: 0, warmup_discarded: trials.length - measured.length };
  result.warmup_outcomes = Object.fromEntries(["verified", "refused", "failed", "quality_failed"].map(outcome => [outcome, trials.filter(trial => trial.warmup && trial.outcome === outcome).length]));
  for (const outcome of ["refused", "failed", "quality_failed"]) result[outcome] = measured.filter((trial) => trial.outcome === outcome).length;
  result.quality_pass_rate = measured.length ? verified.length / measured.length : null;
  result.structured_validity = measured.length ? measured.filter((trial) => trial.grade?.schema_valid).length / measured.length : null;
  result.verified_first_content_ms = distribution(verified.map((trial) => trial.first_content_ms));
  result.verified_elapsed_ms = distribution(verified.map((trial) => trial.elapsed_ms));
  result.all_attempt_elapsed_ms = distribution(measured.map((trial) => trial.elapsed_ms));
  result.output_tokens_per_second = distribution(verified.map((trial) => trial.metrics.output_tokens_per_second));
  result.all_attempt_queue_wait_ms = distribution(measured.map((trial) => finiteNonnegative(trial.admission?.queue_wait_ms)));
  result.all_attempt_total_wait_ms = distribution(measured.map((trial) => finiteNonnegative(trial.admission?.total_wait_ms)));
  const windowValid = measured.length > 0 && measured.every((trial, index) => finiteNonnegative(trial.client_start_ms) !== null && finiteNonnegative(trial.client_end_ms) !== null && trial.client_end_ms >= trial.client_start_ms && (index === 0 || trial.client_start_ms >= measured[index - 1].client_end_ms));
  result.measured_window_ms = windowValid ? measured.at(-1).client_end_ms - measured[0].client_start_ms : null;
  result.measured_window_definition = "First measured client submission through last measured response completion, including inter-request client gaps; warm-ups excluded";
  result.useful_tasks_per_minute = result.measured_window_ms > 0 ? verified.length * 60000 / result.measured_window_ms : null;
  const tokens = { attempts: measured.length, paid_agent_tokens: null, paid_agent_tokens_status: "not_measured", scope: "Local model inference across every measured attempt, including failed effort; warm-ups excluded" };
  const counts = key => measured.map(trial => trial.metrics?.[key]).map(value => Number.isSafeInteger(value) && value >= 0 ? value : null);
  const prompt = counts("prompt_tokens"), output = counts("output_tokens");
  const combined = prompt.map((value, index) => value === null || output[index] === null || !Number.isSafeInteger(value + output[index]) ? null : value + output[index]);
  for (const [name, values] of [["prompt", prompt], ["output", output], ["combined", combined]]) {
    const known = values.filter(value => value !== null);
    tokens[`${name}_known_attempts`] = known.length;
    tokens[`${name}_coverage`] = measured.length ? known.length / measured.length : null;
    const subtotal = known.reduce((sum, value) => sum + value, 0);
    tokens[`known_${name}_total`] = known.length && Number.isSafeInteger(subtotal) ? subtotal : null;
    tokens[`${name}_total`] = measured.length && known.length === measured.length ? tokens[`known_${name}_total`] : null;
    tokens[name === "combined" ? "total_per_verified_completion" : `${name}_per_verified_completion`] = verified.length > 0 && tokens[`${name}_total`] !== null ? tokens[`${name}_total`] / verified.length : null;
  }
  result.local_tokens = tokens;
  if (result.failed || result.refused || result.quality_failed) result.status = "failed";
  return result;
}

const unloadLabel = (unload) => !unload ? null : unload.requested === false ? `skipped: ${unload.skipped_reason}` : unload.confirmed_absent ? `unloaded, confirmed by /api/ps in ${Math.round(unload.elapsed_ms)} ms` : `not confirmed: ${unload.error}`;
const csvCell = (value) => `"${String(value ?? "").replaceAll('"', '""')}"`;
const cell = (value) => value === null || value === undefined ? "—" : typeof value === "number" ? Number(value.toFixed(3)).toString() : String(value).replaceAll("|", "\\|").replaceAll("\n", " ");

export async function writeReports(out, report) {
  report.measured_trials = report.vendors.reduce((count, vendor) => count + (vendor.attempted ?? 0), 0);
  report.publication_status = report.measured_trials === 0 ? "not_run; no measured inference trials" : report.samples < 30 || report.warmups < 1 ? "smoke_only" : "measurement_only; artifact, template, cache, placement, and workload equivalence require review";
  await writeFile(path.join(out, "summary.json"), `${JSON.stringify(report, null, 2)}\n`);
  const columns = ["vendor", "mode", "status", "success", "refused", "failed", "quality_failed", "n", "ttft_p50_ms", "ttft_p95_ms", "ttft_p99_ms", "wall_p50_ms", "wall_p95_ms", "wall_p99_ms", "quality_pass_rate", "queue_wait_p50_ms", "total_wait_p50_ms", "min_available_bytes", "peak_swap_growth_bytes", "ending_swap_growth_bytes", "sampled_warning_ms", "sampled_critical_ms", "sampled_linux_psi_above_10pct_ms", "notes"];
  const values = report.vendors.map((row) => [row.id, row.mode, row.status, row.success, row.refused, row.failed, row.quality_failed, row.attempted ?? 0, row.verified_first_content_ms?.p50, row.verified_first_content_ms?.p95, row.verified_first_content_ms?.p99, row.verified_elapsed_ms?.p50, row.verified_elapsed_ms?.p95, row.verified_elapsed_ms?.p99, row.quality_pass_rate, row.all_attempt_queue_wait_ms?.p50, row.all_attempt_total_wait_ms?.p50, row.host_observations?.min_available_bytes, row.host_observations?.peak_swap_growth_bytes, row.host_observations?.ending_swap_growth_bytes, row.host_observations?.sampled_pressure?.warning_ms, row.host_observations?.sampled_pressure?.critical_ms, row.host_observations?.sampled_pressure?.linux_psi_above_10pct_ms, row.error ?? row.cleanup_error ?? row.sampler_error ?? ""]);
  columns.push("comparison_cohort", "measured_window_ms", "useful_tasks_per_minute", "local_prompt_total", "local_output_total", "local_tokens_per_verified_completion", "prompt_token_coverage", "output_token_coverage", "host_cpu_mean_pct", "host_cpu_peak_pct", "host_cpu_coverage");
  report.vendors.forEach((row, index) => values[index].push(row.comparison_cohort, row.measured_window_ms, row.useful_tasks_per_minute, row.local_tokens?.prompt_total, row.local_tokens?.output_total, row.local_tokens?.total_per_verified_completion, row.local_tokens?.prompt_coverage, row.local_tokens?.output_coverage, row.host_observations?.cpu_busy_percent?.time_weighted_mean, row.host_observations?.cpu_busy_percent?.peak, row.host_observations?.cpu_busy_percent?.coverage));
  columns.push("available_bytes_source", "peak_tracked_rss_bytes", "peak_owned_rss_bytes", "peak_ollama_runner_rss_bytes", "ollama_size_vram_end_bytes", "ollama_size_end_bytes", "unload_outcome", "load1_per_cpu", "load_gate_action");
  report.vendors.forEach((row, index) => values[index].push(row.host_observations?.available_bytes_source, row.host_observations?.process_rss?.peak_tracked_bytes, row.host_observations?.process_rss?.peak_owned_bytes, row.host_observations?.process_rss?.peak_ollama_runner_bytes, row.ollama_residency_end?.size_vram_bytes, row.ollama_residency_end?.size_bytes, unloadLabel(row.unload), row.load_gate?.load1_per_cpu, row.load_gate?.action));
  await writeFile(path.join(out, "summary.csv"), [columns, ...values].map((row) => row.map(csvCell).join(",")).join("\n") + "\n");
  const markdown = ["# Sequential generation benchmark", "", `Date: ${report.started_at}. Scope: S1 warm generation; vendors and requests run one by one.`, "", `Publication status: ${report.publication_status}. This suite does not establish E1 paid-agent cost, cold-load performance, memory safety, managed-task behavior beyond this fixed completion case, or an overall vendor ranking.`, "", "Timings below include only verified answers. Every failure and refusal remains in the counts and raw records. First-token timing begins at request submission and ends at the first nonempty generated content frame; thinking and headers do not count. Managed completion is nonstreaming: its client TTFT stays unavailable, and server timing is reported separately. OpenAI throughput is unavailable unless the server supplies engine timing. p99 is exploratory with fewer than 100 verified samples.", "", "| Vendor | Mode | Status | Verified / attempted | Refused | Failed | Quality failed | TTFT p50 / p95 (ms) | Wall p50 / p95 (ms) |", "|---|---|---|---|---|---|---|---|---|", ...report.vendors.map((row) => `| ${row.id} | ${cell(row.mode)} | ${row.status} | ${row.success ?? 0} / ${row.attempted ?? 0} | ${row.refused ?? 0} | ${row.failed ?? 0} | ${row.quality_failed ?? 0} | ${cell(row.verified_first_content_ms?.p50)} / ${cell(row.verified_first_content_ms?.p95)} | ${cell(row.verified_elapsed_ms?.p50)} / ${cell(row.verified_elapsed_ms?.p95)} |`), "", ...report.vendors.filter((row) => row.error).map((row) => `- ${row.id}: ${cell(row.error)}`), "", "The fixed case is a small extraction smoke check. Its pass rate is not a general model-quality score. Artifact hashes in the config are declarations, not verified hashes. Model bytes, template, engine settings, context, residency, and background workload must be reviewed before any comparative claim. The lock coordinates this runner only; preexisting services and unrelated clients must be checked separately. OpenAI context size and thinking settings require operator confirmation because its standard request cannot enforce Ollama's settings.", ""];
  const scaled = (value, divisor) => typeof value === "number" && Number.isFinite(value) ? value / divisor : null;
  markdown.push("Host observations include warm-ups. Pressure durations use the observed samples, with missing counters and unsampled periods left unknown; a zero is not proof that no brief pressure event occurred. On macOS, available memory is a FreeLlama estimate (free + bounded reclaimable, as the core computes it), not a kernel value; Linux uses MemAvailable. Peak RSS covers the owned process group and Ollama runners. size_vram comes from Ollama /api/ps at block end. RSS and size_vram are never added: on unified memory RSS undercounts GPU buffers.", "", "| Vendor | Min available (GiB) | Available source | Peak swap growth (MiB) | Ending swap growth (MiB) | Warning / critical (ms) | Linux PSI above 10% (ms) | Peak RSS (MiB) | Ollama size_vram at end (MiB) |", "|---|---|---|---|---|---|---|---|---|");
  for (const row of report.vendors) {
    const host = row.host_observations;
    markdown.push(`| ${row.id} | ${cell(scaled(host?.min_available_bytes, 1024 ** 3))} | ${cell(host?.available_bytes_source)} | ${cell(scaled(host?.peak_swap_growth_bytes, 1024 ** 2))} | ${cell(scaled(host?.ending_swap_growth_bytes, 1024 ** 2))} | ${cell(host?.sampled_pressure?.warning_ms)} / ${cell(host?.sampled_pressure?.critical_ms)} | ${cell(host?.sampled_pressure?.linux_psi_above_10pct_ms)} | ${cell(scaled(host?.process_rss?.peak_tracked_bytes, 1024 ** 2))} | ${cell(scaled(row.ollama_residency_end?.size_vram_bytes, 1024 ** 2))} |`);
  }
  markdown.push("");
  const gated = report.vendors.filter(row => row.load_gate);
  if (gated.length) markdown.push("## Load at block start", "", "| Vendor | Load per CPU (1 min) | Limit | Action | Top processes |", "|---|---|---|---|---|", ...gated.map(row => `| ${row.id} | ${cell(row.load_gate.load1_per_cpu)} | ${cell(row.load_gate.max_load_per_cpu)} | ${row.load_gate.action} | ${cell(row.load_gate.top_processes?.map(top => `${top.command} ${top.pcpu}%`).join(", "))} |`), "");
  const unloaded = report.vendors.filter(row => row.unload);
  if (unloaded.length) markdown.push("## Unload after block", "", ...unloaded.map(row => `- ${row.id}: ${cell(unloadLabel(row.unload))}`), "");
  const levels = report.vendors.filter(row => row.concurrency?.length);
  if (levels.length) markdown.push("## Concurrency (C1)", "", "Each level sends the sample count through N concurrent clients on the warm model. Throughput uses wall time from first submission to last completion.", "", "| Vendor | Clients | Verified / attempted | Failures | Wall p50 / p95 (ms) | TTFT p50 / p95 (ms) | Wall window (ms) | Verified/min |", "|---|---|---|---|---|---|---|---|", ...levels.flatMap(row => row.concurrency.map(level => `| ${row.id} | ${level.level} | ${level.success} / ${level.attempted} | ${level.failures} | ${cell(level.verified_elapsed_ms.p50)} / ${cell(level.verified_elapsed_ms.p95)} | ${cell(level.verified_first_content_ms.p50)} / ${cell(level.verified_first_content_ms.p95)} | ${cell(level.wall_window_ms)} | ${cell(level.wall_verified_per_minute)} |`)), "");
  const colds = report.vendors.filter(row => row.cold);
  if (colds.length) markdown.push("## Cold load (C3)", "", "Once per block after warm samples: unload (Ollama keep_alive 0, confirmed by /api/ps) or restart the owned process, then time one request.", "", "| Vendor | Method | Status | Verified / attempted | First request p50 / p95 (ms) | Vendor load p50 (ms) | Restart to ready p50 (ms) |", "|---|---|---|---|---|---|---|", ...colds.map(row => `| ${row.id} | ${cell(row.cold.method)} | ${cell(row.cold.status)}${row.cold.reason ? `: ${cell(row.cold.reason)}` : ""} | ${row.cold.success} / ${row.cold.attempted} | ${cell(row.cold.verified_elapsed_ms.p50)} / ${cell(row.cold.verified_elapsed_ms.p95)} | ${cell(row.cold.load_ms.p50)} | ${cell(row.cold.restart_to_ready_ms.p50)} |`), "");
  const hogs = report.vendors.filter(row => row.memory_hog);
  if (hogs.length) markdown.push("## Memory hog (C2)", "", "| Vendor | Requested (GiB) | Allowed | Swap before / after ready / after stop (MiB) |", "|---|---|---|---|", ...hogs.map(row => `| ${row.id} | ${cell(scaled(row.memory_hog.requested_bytes, 1024 ** 3))} | ${row.memory_hog.allowed ? row.memory_hog.forced ? "forced" : "yes" : `no: ${cell(row.memory_hog.reason)}`} | ${cell(scaled(row.memory_hog.swap_before_bytes, 1024 ** 2))} / ${cell(scaled(row.memory_hog.swap_after_ready_bytes, 1024 ** 2))} / ${cell(scaled(row.memory_hog.swap_after_stop_bytes, 1024 ** 2))} |`), "");
  await writeFile(path.join(out, "SUMMARY.md"), markdown.join("\n"));
  if (levels.length) {
    const levelColumns = ["vendor", "clients", "attempted", "verified", "refused", "failed", "quality_failed", "wall_p50_ms", "wall_p95_ms", "ttft_p50_ms", "ttft_p95_ms", "wall_window_ms", "verified_per_minute"];
    const levelRows = levels.flatMap(row => row.concurrency.map(level => [row.id, level.level, level.attempted, level.success, level.refused, level.failed, level.quality_failed, level.verified_elapsed_ms.p50, level.verified_elapsed_ms.p95, level.verified_first_content_ms.p50, level.verified_first_content_ms.p95, level.wall_window_ms, level.wall_verified_per_minute]));
    await writeFile(path.join(out, "concurrency.csv"), [levelColumns, ...levelRows].map((row) => row.map(csvCell).join(",")).join("\n") + "\n");
  }
  await writeFile(path.join(out, "report.html"), renderDashboard(report));
}

const outcomeCounts = (trials) => Object.fromEntries(["verified", "refused", "failed", "quality_failed"].map(outcome => [outcome, trials.filter(trial => trial.outcome === outcome).length]));

// C1: one level's concurrent clients share the same case and model. Throughput uses wall time
// from the first submission to the last completion, because requests overlap.
export function summarizeConcurrency(level, trials) {
  const verified = trials.filter(trial => trial.outcome === "verified");
  const starts = trials.map(trial => trial.client_start_ms).filter(value => finiteNonnegative(value) !== null);
  const ends = trials.map(trial => trial.client_end_ms).filter(value => finiteNonnegative(value) !== null);
  const wall = starts.length && ends.length && starts.length === trials.length && ends.length === trials.length ? Math.max(...ends) - Math.min(...starts) : null;
  const counts = outcomeCounts(trials);
  return { level, attempted: trials.length, success: counts.verified, refused: counts.refused, failed: counts.failed, quality_failed: counts.quality_failed, failures: trials.length - counts.verified, verified_elapsed_ms: distribution(verified.map(trial => trial.elapsed_ms)), verified_first_content_ms: distribution(verified.map(trial => trial.first_content_ms)), all_attempt_queue_wait_ms: distribution(trials.map(trial => finiteNonnegative(trial.admission?.queue_wait_ms))), wall_window_ms: wall, wall_verified_per_minute: wall > 0 ? counts.verified * 60000 / wall : null };
}
// C3: each cold sample unloads first, then times one request. load_ms comes from the vendor.
export function summarizeCold(trials, { method = null, status = "complete", reason = null } = {}) {
  const verified = trials.filter(trial => trial.outcome === "verified");
  const counts = outcomeCounts(trials);
  return { status, reason, method, attempted: trials.length, success: counts.verified, refused: counts.refused, failed: counts.failed, quality_failed: counts.quality_failed, unload_confirmed: trials.filter(trial => trial.cold_preparation?.unload?.confirmed_absent).length, verified_elapsed_ms: distribution(verified.map(trial => trial.elapsed_ms)), verified_first_content_ms: distribution(verified.map(trial => trial.first_content_ms)), load_ms: distribution(trials.map(trial => trial.metrics?.load_ms)), restart_to_ready_ms: distribution(trials.map(trial => trial.cold_preparation?.restart_to_ready_ms)) };
}

export function validateRunOptions({ concurrency = [], cold = false, coldSamples = 3, hogGb, forceHog = false, maxLoadPerCpu, strictLoad = false } = {}) {
  if (!Array.isArray(concurrency) || concurrency.some(level => !Number.isSafeInteger(level) || level < 1 || level > 64) || new Set(concurrency).size !== concurrency.length) throw new Error("concurrency levels must be unique integers from 1 to 64.");
  if (!Number.isSafeInteger(coldSamples) || coldSamples < 1 || coldSamples > 100) throw new Error("cold samples must be an integer from 1 to 100.");
  if (hogGb !== undefined) parseHogGb(hogGb);
  if (hogGb !== undefined && !cold) throw new Error("A memory hog requires cold mode (--cold).");
  if (forceHog && hogGb === undefined) throw new Error("--force-hog requires --hog-gb.");
  if (maxLoadPerCpu !== undefined && (typeof maxLoadPerCpu !== "number" || !Number.isFinite(maxLoadPerCpu) || maxLoadPerCpu <= 0)) throw new Error("max load per CPU must be a positive number.");
  if (strictLoad && maxLoadPerCpu === undefined) throw new Error("--strict-load requires --max-load-per-cpu.");
}

export async function runBenchmark({ config, configText = JSON.stringify(config), out, samples = 30, warmups = 2, allowSmoke = false, vendorId, lockPath = DEFAULT_LOCK_PATH, signal, fetchImpl = fetch, startProcessImpl = ownedProcess, onProgress = (message) => console.error(message),
  observeOllama = false, unloadAfterBlock = false, maxLoadPerCpu, strictLoad = false, concurrency = [], cold = false, coldSamples = 3, hogGb, forceHog = false,
  loadProbe = loadSnapshot, processSampler = sampleProcessRss, hostProbe = sampleHost, startHogImpl = startMemoryHog }) {
  validateConfig(config);
  if (!Number.isSafeInteger(samples) || samples < 1 || !Number.isSafeInteger(warmups) || warmups < 0) throw new Error("samples must be positive and warmups nonnegative integers.");
  if (samples < 30 && !allowSmoke) throw new Error("Fewer than 30 samples requires --allow-smoke; the output will not qualify a performance claim.");
  validateRunOptions({ concurrency, cold, coldSamples, hogGb, forceHog, maxLoadPerCpu, strictLoad });
  // Level 1 is the normal sequential pass, which always runs.
  const levels = concurrency.filter(level => level > 1);
  const vendors = config.vendors.filter((vendor) => vendor.enabled !== false && (!vendorId || vendor.id === vendorId));
  if (!vendors.length) throw new Error("No enabled vendor matches the selection.");
  try { await stat(out); throw new Error(`Output already exists: ${out}. Use a new directory; results are never overwritten.`); } catch (error) { if (error.code !== "ENOENT") throw error; }
  const ollamaBase = config.ollama_base_url ?? DEFAULT_OLLAMA_BASE_URL;
  // Models this run made resident. Only these may be unloaded; a model that was already
  // resident before its block belongs to someone else and is left alone.
  const warmedByRun = new Set();
  const unlock = await acquireLock(lockPath);
  try {
    await mkdir(path.dirname(out), { recursive: true });
    await mkdir(out);
    const report = { schema_version: 1, started_at: new Date().toISOString(), finished_at: null, host: { hostname: os.hostname(), platform: process.platform, arch: process.arch, os_release: os.release(), node: process.version }, config_sha256: sha256(configText), case_sha256: sha256(JSON.stringify(CASE)), samples, warmups, publication_status: samples < 30 || warmups < 1 ? "smoke_only" : "measurement_only; artifact, template, cache, placement, and workload equivalence require review", scope: "S1 warm sequential generation; FreeLlama raw admission and managed completion reported separately", vendors: [] };
    const coverage = { ...(levels.length ? { concurrency_levels: levels } : {}), ...(cold ? { cold: { samples_per_block: coldSamples, method: "Once per block after warm samples: Ollama-backed vendors unload with keep_alive 0 (confirmed by /api/ps); owned processes restart; then one request is timed per cold sample." } } : {}), ...(hogGb !== undefined ? { memory_hog_gb: hogGb, force_hog: forceHog } : {}), ...(maxLoadPerCpu !== undefined ? { max_load_per_cpu: maxLoadPerCpu, strict_load: strictLoad } : {}), ...(observeOllama || unloadAfterBlock ? { ollama: { base_url: ollamaBase, observe_residency: observeOllama, unload_after_block: unloadAfterBlock } } : {}) };
    if (Object.keys(coverage).length) report.coverage = coverage;
    // Config may contain operator-provided environment values; keep only their names in evidence.
    await writeFile(path.join(out, "config.redacted.json"), `${JSON.stringify({ ...config, vendors: config.vendors.map((vendor) => ({ ...vendor, ...(vendor.start ? { start: { ...vendor.start, env: Object.fromEntries(Object.keys(vendor.start.env ?? {}).map((key) => [key, "[redacted]"])) } } : {}) })) }, null, 2)}\n`);
    await writeReports(out, report);
    for (const vendor of vendors) {
      if (signal?.aborted) {
        for (const remaining of vendors.slice(report.vendors.length)) report.vendors.push({ ...summarizeVendor(remaining, [], config.model), status: "blocked", error: "Benchmark was cancelled before this vendor started." });
        report.stop_reason = String(signal.reason?.message ?? signal.reason ?? "Cancelled");
        break;
      }
      const directory = path.join(out, vendor.id);
      await mkdir(directory);
      const ollamaBacked = OLLAMA_BACKED.has(vendor.protocol);
      const model = vendor.model ?? config.model.ollama_tag;
      const extras = {};
      // Gates that refuse a block record it as not run, never as failed.
      const notRun = async (reason) => {
        report.vendors.push({ ...summarizeVendor(vendor, [], config.model), status: "not_run", error: reason, host_observations: null, ...extras });
        await writeReports(out, report);
        onProgress(`Skipped ${vendor.id}: ${reason}`);
      };
      if (maxLoadPerCpu !== undefined) {
        const snapshot = await loadProbe();
        extras.load_gate = { ...snapshot, ...evaluateLoadGate(snapshot, maxLoadPerCpu, strictLoad) };
        await writeFile(path.join(directory, "load-gate.json"), `${JSON.stringify(extras.load_gate, null, 2)}\n`);
        const top = extras.load_gate.top_processes.map(row => `${row.command} ${row.pcpu}%`).join(", ");
        if (extras.load_gate.action === "warn") onProgress(`Warning: ${vendor.id} starts at load ${extras.load_gate.load1_per_cpu.toFixed(2)} per CPU (limit ${maxLoadPerCpu}). Top: ${top}`);
        if (extras.load_gate.action === "refuse") { await notRun(`Load gate: 1-minute load ${extras.load_gate.load1_per_cpu.toFixed(2)} per CPU exceeds ${maxLoadPerCpu}. Top: ${top}`); continue; }
      }
      if (hogGb !== undefined) {
        const host = await hostProbe();
        extras.memory_hog = { ...hogDecision({ gb: hogGb, availableBytes: host.available_bytes, force: forceHog }), available_source: host.available_bytes_source ?? null, swap_before_bytes: host.swap_used_bytes ?? null };
        if (!extras.memory_hog.allowed) { await notRun(`Memory hog refused: ${extras.memory_hog.reason}. Use --force-hog to override.`); continue; }
      }
      const trials = [];
      let owned = { stop: async () => {} };
      let stopSampler = async () => {};
      let hog = null;
      let vendorReport;
      let cleanupFailed = false;
      let executionUncertain = false;
      let blockStarted = false;
      const startOwned = async (logName) => {
        const begun = performance.now();
        owned = await startProcessImpl(vendor, directory, { logName });
        const deadline = Date.now() + (vendor.start.ready_timeout_ms ?? 30_000);
        for (;;) {
          owned.assertRunning();
          try { await health(vendor, config.model, { fetchImpl, signal }); break; } catch (error) {
            if (Date.now() >= deadline || signal?.aborted) throw error;
            await delay(250, signal);
          }
        }
        return performance.now() - begun;
      };
      // Concurrent clients share one ordered writer so large lines never interleave.
      let writes = Promise.resolve();
      const record = async (file, trial) => {
        executionUncertain ||= trial.execution_uncertain;
        const write = writes.then(() => appendFile(path.join(directory, file), `${JSON.stringify(trial)}\n`));
        writes = write.catch(() => {});
        await write;
        if (trial.execution_uncertain) throw new Error("Inference outcome is uncertain; no further samples or vendors will be submitted.");
      };
      onProgress(`Starting ${vendor.id}; no other vendor is measured concurrently.`);
      try {
        if (hogGb !== undefined) {
          hog = await startHogImpl({ gb: hogGb, force: forceHog, signal });
          extras.memory_hog.ready = hog.ready ?? null;
          extras.memory_hog.swap_after_ready_bytes = (await hostProbe()).swap_used_bytes ?? null;
        }
        if (ollamaBacked && (observeOllama || unloadAfterBlock)) extras.ollama_residency_before = await ollamaResidency(ollamaBase, model, { fetchImpl, signal });
        if (vendor.start) {
          // Refuse to attach a configured start command to an already responding listener.
          let responding = false;
          try { await fetchImpl(vendor.base_url, { redirect: "error", signal: AbortSignal.timeout(1_000) }); responding = true; } catch { /* A new owned service may be started. */ }
          if (responding) throw new Error("Endpoint already responds; remove start to use that existing service. It will not be stopped.");
          await startOwned("server.log");
        }
        const before = await health(vendor, config.model, { fetchImpl, signal });
        await writeFile(path.join(directory, "health-before.json"), `${JSON.stringify(before, null, 2)}\n`);
        stopSampler = await startHostSampler(path.join(directory, "host.jsonl"), { intervalMs: 1000, processSampler: () => processSampler({ ownedPgid: owned.child?.pid ?? null, includeOllamaRunners: ollamaBacked }) });
        blockStarted = true;
        for (let index = 0; index < warmups + samples; index++) {
          if (signal?.aborted) throw signal.reason;
          owned.assertRunning?.();
          const trial = { vendor: vendor.id, case_id: CASE.id, index, warmup: index < warmups, started_at: new Date().toISOString(), ...await measureRequest(vendor, config.model, { fetchImpl, signal }) };
          trials.push(trial);
          // Preserve the halt decision even if saving the receipt subsequently fails.
          await record("trials.jsonl", trial);
          onProgress(`${vendor.id}: ${trial.warmup ? "warmup" : "sample"} ${trial.warmup ? index + 1 : index + 1 - warmups}/${trial.warmup ? warmups : samples}: ${trial.outcome}`);
          // A failed warm-up is a prerequisite failure, not a discarded failure that silently vanishes.
          if (trial.warmup && trial.outcome !== "verified") throw new Error(`Warm-up ${index + 1} ${trial.outcome}: ${trial.error ?? "answer failed its exact extraction check"}`);
        }
        if (levels.length) extras.concurrency = [];
        for (const level of levels) {
          const levelTrials = [];
          let next = 0, halt = false;
          const client = async (clientIndex) => {
            try {
              while (!halt && next < samples) {
                signal?.throwIfAborted();
                owned.assertRunning?.();
                const index = next++;
                const trial = { vendor: vendor.id, case_id: CASE.id, concurrency: level, client: clientIndex, index, warmup: false, started_at: new Date().toISOString(), ...await measureRequest(vendor, config.model, { fetchImpl, signal }) };
                levelTrials.push(trial);
                await record(`concurrency-${level}.jsonl`, trial);
              }
            } catch (error) { halt = true; throw error; }
          };
          // Let in-flight clients finish before reporting the first failure.
          const settled = await Promise.allSettled(Array.from({ length: level }, (_, clientIndex) => client(clientIndex)));
          extras.concurrency.push(summarizeConcurrency(level, levelTrials));
          onProgress(`${vendor.id}: concurrency ${level}: ${levelTrials.filter(trial => trial.outcome === "verified").length}/${levelTrials.length} verified`);
          const rejected = settled.find(result => result.status === "rejected");
          if (rejected) throw rejected.reason;
        }
        if (cold) {
          const coldTrials = [];
          const method = ollamaBacked ? "ollama_keep_alive_0" : vendor.start ? "owned_restart" : null;
          extras.cold = summarizeCold([], { method, status: method ? "complete" : "unsupported", reason: method ? null : "Not Ollama-backed and no start command; the harness cannot unload this vendor's model." });
          for (let index = 0; method && index < coldSamples; index++) {
            signal?.throwIfAborted();
            const preparation = { method, unload: null, restart_to_ready_ms: null };
            if (method === "ollama_keep_alive_0") {
              preparation.unload = await unloadOllamaModel(ollamaBase, model, { fetchImpl, signal });
              if (!preparation.unload.confirmed_absent) { extras.cold = summarizeCold(coldTrials, { method, status: "incomplete", reason: `Unload was not confirmed: ${preparation.unload.error}` }); break; }
            } else {
              try { await owned.stop(); } catch (error) { cleanupFailed = true; throw error; }
              owned = { stop: async () => {} };
              preparation.restart_to_ready_ms = await startOwned(`server-cold-${index + 1}.log`);
            }
            const trial = { vendor: vendor.id, case_id: CASE.id, index, warmup: false, cold: true, cold_preparation: preparation, started_at: new Date().toISOString(), ...await measureRequest(vendor, config.model, { fetchImpl, signal }) };
            coldTrials.push(trial);
            extras.cold = summarizeCold(coldTrials, { method });
            await record("cold.jsonl", trial);
            onProgress(`${vendor.id}: cold ${index + 1}/${coldSamples}: ${trial.outcome}`);
          }
        }
        vendorReport = { ...summarizeVendor(vendor, trials, config.model), version: before.version, upstream_version: before.upstream_version ?? null };
      } catch (error) {
        vendorReport = { ...summarizeVendor(vendor, trials, config.model), status: trials.length ? "failed" : "unavailable", error: String(error.message ?? error) };
        await writeFile(path.join(directory, "health-error.json"), `${JSON.stringify({ error: vendorReport.error, observation: error.observation ?? null }, null, 2)}\n`);
      } finally {
        try { await stopSampler(); } catch (error) { vendorReport = { ...vendorReport, status: "failed", sampler_error: String(error.message ?? error) }; }
        if (blockStarted && ollamaBacked && observeOllama) extras.ollama_residency_end = await ollamaResidency(ollamaBase, model, { fetchImpl });
        if (blockStarted && ollamaBacked && extras.ollama_residency_before?.resident === false) warmedByRun.add(model);
        if (blockStarted && ollamaBacked && unloadAfterBlock) {
          // Never unload under an uncertain inference, or a model this run did not warm.
          const skip = executionUncertain ? "execution_uncertain" : warmedByRun.has(model) ? null : extras.ollama_residency_before?.resident === true ? "resident_before_block_not_warmed_by_this_run" : "residency_before_block_unknown";
          extras.unload = skip ? { model, requested: false, skipped_reason: skip } : await unloadOllamaModel(ollamaBase, model, { fetchImpl });
          if (extras.unload.confirmed_absent) warmedByRun.delete(model);
          onProgress(`${vendor.id}: unload ${model}: ${skip ?? (extras.unload.confirmed_absent ? "confirmed by /api/ps" : `not confirmed (${extras.unload.error})`)}`);
        }
        try { await owned.stop(); } catch (error) { cleanupFailed = true; vendorReport = { ...vendorReport, status: "failed", cleanup_error: String(error.message ?? error) }; }
        if (hog) {
          try { extras.memory_hog.stop = await hog.stop(); } catch (error) { cleanupFailed = true; vendorReport = { ...vendorReport, status: "failed", cleanup_error: `Memory hog: ${error.message ?? error}` }; }
          try { extras.memory_hog.swap_after_stop_bytes = (await hostProbe()).swap_used_bytes ?? null; } catch { /* Observation only. */ }
        }
        vendorReport.host_observations = null;
        try {
          const hostSamples = (await readFile(path.join(directory, "host.jsonl"), "utf8")).trim().split("\n").filter(Boolean).map(JSON.parse);
          vendorReport.host_observations = summarizeHost(hostSamples);
        } catch (error) { if (error.code !== "ENOENT") vendorReport.host_observation_error = String(error.message ?? error); }
        Object.assign(vendorReport, extras);
      }
      report.vendors.push(vendorReport);
      await writeReports(out, report);
      onProgress(`Finished ${vendor.id}: ${vendorReport.status}.`);
      if (cleanupFailed || executionUncertain || signal?.aborted) {
        report.stop_reason = cleanupFailed ? "Owned server cleanup failed." : signal?.aborted ? "Benchmark was cancelled." : "An uncertain inference may still run upstream; frontend teardown cannot establish upstream quiescence.";
        for (const remaining of vendors.slice(report.vendors.length)) report.vendors.push({ ...summarizeVendor(remaining, [], config.model), status: "blocked", error: `${report.stop_reason} This vendor was not started.` });
        break;
      }
    }
    report.finished_at = new Date().toISOString();
    await writeReports(out, report);
    return report;
  } finally { await unlock(); }
}
