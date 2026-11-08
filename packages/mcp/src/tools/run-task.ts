// run_task: preview, warm, single, and batch execution through managed serve.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { cancelTaskJob, getTaskJob, route, runTaskRequest } from "../serve-client.js";
import { withBatchTelemetry, withTaskTelemetry } from "../telemetry.js";
import { runWithJobProgress, type CancellationState } from "../job-progress.js";
import { notice, progressSink, taskNotices, type ToolExtra } from "../progress.js";
import { ensureServe } from "../serve.js";
import { deferText, routeText, withText } from "../result-text.js";
import {
  BATCH_TASK_FIELDS, type BatchItem, batchItemParam, embeddingInputParam, endpointParam, errorResult, executionPreferenceParam, filesParam,
  isStructuredSuccess, localToolsParam, messagesParam, minConfidenceParam, minPlacementEvidenceParam, objectiveParam,
  parsedResult, questionsParam, requiredCapabilitiesParam, stateParam, summarizeEmbeddings, systemPromptParam,
  parseBatchTask, taskAnswerText, taskMessages, taskParam, withRequiredCapability,
} from "../helpers.js";
import { applyTaskFiles, inputLabels, type FileReceipt } from "../task-files.js";
import { nearestText, validateEmbeddingVectors, type NearestRequest } from "../nearest.js";

// Typed where the agent acts on a field; Ollama-owned payloads (`response`) stay open.
const taskResultSchema = z.object({
  selected_model: z.string().optional(),
  context_window_fit: z.string().optional(),
  execution: z.unknown().optional(),
  response: z.unknown().optional(),
  nearest: z.object({
    mode: z.enum(["query", "pairs"]),
    matches: z.array(z.object({ label: z.string(), score: z.number() }).loose()).optional(),
    pairs: z.array(z.object({ score: z.number() }).loose()).optional(),
  }).optional(),
  files_read: z.array(z.object({ path: z.string(), role: z.string() }).loose()).optional(),
  deferred: z.boolean().optional(),
  job: z.object({ id: z.string(), status: z.string() }).loose().optional(),
  response_file: z.string().optional(),
  sections: z.array(z.string()).optional(),
  telemetry: z.unknown().optional(),
}).loose();
const batchResultSchema = z.object({
  results: z.array(z.object({
    id: z.string(),
    ok: z.boolean(),
    response: z.unknown().optional(),
    error: z.unknown().optional(),
  }).loose()).optional(),
  deferred: z.boolean().optional(),
  jobs: z.array(z.object({
    id: z.string(),
    job: z.object({ id: z.string(), status: z.string() }).loose().optional(),
    error: z.unknown().optional(),
  }).loose()).optional(),
  telemetry: z.unknown().optional(),
}).loose();

type TaskPayload = Pick<BatchItem["task"],
  "task" | "scopeId" | "scopeRevision" | "prompt" | "systemPrompt" | "messages" | "images" | "input" | "tools" |
  "format" | "think" | "logprobs" | "topLogprobs" | "options" | "state" | "questions"> &
  { returnEmbeddings?: boolean; nearest?: NearestRequest };

/**
 * Payload rules shared by single and batch execution, checked after `files` filled the payload.
 * Returns the first problem, prefixed with `label` (the batch item) when given, or null.
 */
function validateTaskPayload(task: TaskPayload, label?: string): string | null {
  const fail = (message: string) => (label ? `${label}: ${message}` : message);
  if ((task.scopeId === undefined) !== (task.scopeRevision === undefined)) {
    return fail("scopeId and scopeRevision must be supplied together.");
  }
  if (task.topLogprobs !== undefined && task.logprobs !== true) return fail("`topLogprobs` requires `logprobs:true`.");
  if (task.nearest !== undefined && task.task !== "embedding") return fail('`nearest` ranks embeddings; it is valid only for task "embedding".');
  if (task.task === "decision") {
    if (task.state === undefined) return fail('task "decision" requires `state` (text or JSON), or one text file in `files`.');
    if (task.questions === undefined || Object.keys(task.questions).length === 0) {
      return fail('task "decision" requires `questions`: {"<name>": {"type":"noul"|"choice"|"score","instructions":"...","criteria":...}}.');
    }
    const chatFields = Object.entries({
      prompt: task.prompt, systemPrompt: task.systemPrompt, messages: task.messages, input: task.input, tools: task.tools,
      format: task.format, think: task.think, logprobs: task.logprobs, topLogprobs: task.topLogprobs, options: task.options,
      returnEmbeddings: task.returnEmbeddings,
    }).filter(([, value]) => value !== undefined).map(([name]) => `\`${name}\``);
    if (chatFields.length > 0) return fail(`task "decision" takes \`state\`, \`questions\` and optional \`images\`; remove ${chatFields.join(", ")}.`);
    return null;
  }
  if (task.state !== undefined || task.questions !== undefined) {
    return fail('`state` and `questions` are valid only for task "decision".');
  }
  if (task.task === "embedding") {
    if (task.input === undefined) return fail('task "embedding" requires `input` or text `files`.');
    const chatControls = [task.prompt, task.systemPrompt, task.images, task.messages, task.tools, task.format, task.think,
      task.logprobs, task.topLogprobs];
    if (chatControls.some((value) => value !== undefined)) {
      return fail('task "embedding" accepts `input`, `files`, `nearest`, `options`, and routing fields; chat controls are not valid.');
    }
    return null;
  }
  if (task.input !== undefined || task.returnEmbeddings !== undefined) {
    return fail('`input` and `returnEmbeddings` are valid only for task "embedding".');
  }
  if (task.prompt === undefined && task.systemPrompt === undefined && (task.messages === undefined || task.messages.length === 0)) {
    return fail(`task "${task.task ?? "completion"}" requires \`prompt\`, \`systemPrompt\`, or \`messages\`.`);
  }
  if (task.images !== undefined && (task.messages !== undefined || task.prompt === undefined)) {
    return fail("Top-level `images` is valid only with `prompt`; put images inside messages instead.");
  }
  return null;
}

/** Fill `prompt`/`images`/`input`/`state` from `files`; a read problem becomes the caller's error. */
function withFiles<T extends TaskPayload & { files?: string[] }>(task: T, label?: string): { task: T; files: FileReceipt[] } {
  try {
    const { payload, files } = applyTaskFiles(task, task.files);
    const { files: _paths, documentPrefix: _prefix, ...rest } = payload;
    return { task: rest as T, files };
  } catch (error) {
    throw new Error(label ? `${label}: ${(error as Error).message}` : (error as Error).message);
  }
}

/**
 * Top-level fields that, with `tasks`, are defaults for every item: the parts a batch repeats
 * (the same questions for 40 issues, one system prompt for 50 rewrites) are sent once.
 */
export const BATCH_SHARED_FIELDS: ReadonlySet<string> = new Set([
  "task", "model", "objective", "sessionId", "contextTokens", "executionPreference", "systemPrompt", "questions",
  "format", "think", "options", "keepAlive", "priority", "timeoutSeconds", "maxWaitSeconds", "stopOnRepetition", "tools",
  "minConfidence", "requiredCapabilities", "minPlacementEvidence",
]);

function withoutUndefined<T extends object>(value: T): Partial<T> {
  return Object.fromEntries(Object.entries(value).filter(([, entry]) => entry !== undefined)) as Partial<T>;
}

type Fields = Record<string, unknown>;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;

/**
 * `cancellation` is what cancelling the accepted job achieved: `acknowledged` or `requested` mean
 * serve received the request; `unknown` means no usable acknowledgement arrived, so the job may still be running.
 */
function cancellationError(jobId?: string, cancellation: CancellationState = "requested"): Error {
  return new Error(JSON.stringify({
    code: "task_cancelled", error: !jobId
      ? "Client cancelled before the next task was submitted."
      : cancellation === "unknown"
        ? `Client cancelled; cancellation acknowledgement is unknown for accepted job ${jobId}, which may still be running. The task was not run again.`
        : `Client cancelled; cancellation ${cancellation === "acknowledged" ? "acknowledged by serve" : "requested"} for accepted job ${jobId}. The task was not run again.`,
    ...(jobId ? { job_id: jobId, cancellation_requested: true, cancellation_state: cancellation } : {}),
    execution_outcome: jobId ? "unknown" : "not_started", retry_safe: jobId === undefined,
  }));
}

/**
 * Every managed task runs as a cancellable job: the MCP SDK gives each request handler a signal,
 * whether or not the client subscribed to progress. Accepted jobs are never replayed.
 */
async function runManagedTask(
  target: string, request: Fields, report: ((message: string) => void) | undefined, signal: AbortSignal,
  onJob?: (id: string) => void,
): Promise<string> {
  if (signal.aborted) throw cancellationError();
  if (request.defer === true) return runTaskRequest(target, request);
  let acceptedId: string | undefined;
  try {
    const raw = await runWithJobProgress({
      submit: () => runTaskRequest(target, { ...request, defer: true }),
      get: (id) => getTaskJob(target, id),
      cancel: (id) => cancelTaskJob(target, id),
      sync: (refusal) => {
        // This callback is reached only for a definitive pre-execution job refusal. The request
        // must stay cancellable; switching to an uninterruptible native request is unsafe.
        // Serve's own refusal says which limit applied (input size, job count, input budget).
        throw new Error(JSON.stringify({
          ...refusal,
          error: `Serve refused the job before execution: ${typeof refusal?.error === "string" ? refusal.error : "no job capacity or unsupported request size"}`,
          code: typeof refusal?.code === "string" ? refusal.code : "cancellable_task_unavailable",
          execution_outcome: "not_started", retry_safe: true,
        }));
      },
    }, report ?? (() => undefined), { signal, aborted: (id, cancellation) => id ? cancellationError(id, cancellation) : new Error(JSON.stringify({
      code: "task_cancelled", error: "Client cancelled while submitting; no job handle was received and execution outcome is unknown. The task was not replayed.",
      execution_outcome: "unknown", retry_safe: false,
    })), onJob: (id, open) => {
      if (open) { acceptedId = id; onJob?.(id); }
    } });
    const parsed = object(JSON.parse(raw));
    if (parsed?.deferred === true) throw new Error(JSON.stringify({
      code: "task_job_acknowledgement_invalid", error: "Serve returned an incomplete deferred acknowledgement; execution outcome is unknown and the task was not replayed.",
      execution_outcome: "unknown", retry_safe: false,
    }));
    if (!acceptedId) return raw;
    return parsed ? JSON.stringify({ ...parsed, accepted_job_ids: [acceptedId] }) : raw;
  } catch (error) {
    if (!acceptedId) throw error;
    let cause: Fields | undefined;
    try { cause = object(JSON.parse(error instanceof Error ? error.message : String(error))); } catch { /* transport failure */ }
    throw new Error(JSON.stringify({
      code: "task_job_failed", error: error instanceof Error ? error.message : String(error), ...cause,
      job_id: acceptedId, retry_safe: false,
    }));
  }
}

function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const fields = object(value);
  if (fields) return `{${Object.keys(fields).sort().map((key) => `${JSON.stringify(key)}:${canonical(fields[key])}`).join(",")}}`;
  return JSON.stringify(value);
}

function sliceIdentity(receipt: Fields, dimensions: number): Fields {
  const identity = object(receipt.embedding_identity);
  const effective = object(identity?.effective_options);
  // A task receipt names its model in `route` (the routing decision); only previews have it top-level.
  const selectedModel = object(receipt.route)?.selected_model;
  if (identity?.status !== "verified" || typeof selectedModel !== "string" || !selectedModel
    || typeof identity.model_digest !== "string" || !identity.model_digest
    || !["cpu", "primary"].includes(String(identity.backend))
    || typeof identity.upstream !== "string" || !identity.upstream
    || identity.dimensions !== dimensions || !effective || !object(effective.options)
    || typeof effective.truncate !== "boolean"
    || !(effective.dimensions === null || (Number.isInteger(effective.dimensions) && Number(effective.dimensions) > 0))
    || !(effective.observed_context_tokens === null || (Number.isInteger(effective.observed_context_tokens) && Number(effective.observed_context_tokens) > 0))) {
    throw new Error("embedding identity is uncertain: every slice requires a verified model digest, backend, upstream, dimensions and effective options receipt");
  }
  if (!(Number(effective.observed_context_tokens) > 0 || Number(object(effective.options)?.num_ctx) > 0)) {
    throw new Error("embedding identity is uncertain: effective context was not observed or explicitly fixed");
  }
  return { ...identity, selected_model: selectedModel };
}

/** Sum only complete counters; a missing measurement remains unknown instead of becoming zero. */
function sumFields(receipts: Fields[], field: string, keys: string[]): Fields {
  return Object.fromEntries(keys.map((key) => {
    const values = receipts.map((receipt) => object(receipt[field])?.[key]);
    const sum = values.every((value) => typeof value === "number" && Number.isFinite(value) && value >= 0)
      ? (values as number[]).reduce((total, value) => total + value, 0) : null;
    return [key, sum !== null && Number.isFinite(sum) ? sum : null];
  }));
}

/** Embed sequentially, preserving one embedding space and all completed execution receipts. */
export async function embedInSlices(
  target: string,
  request: Fields,
  input: unknown[],
  report: ((message: string) => void) | undefined,
  signal: AbortSignal,
): Promise<string> {
  const vectors: number[][] = [];
  const receipts: Fields[] = [];
  const acceptedJobs: string[] = [];
  let identity: Fields | undefined;
  let first: Fields | undefined;
  const limit = EMBEDDINGS_PER_JOB;
  const deadline = typeof request.timeout_seconds === "number" ? performance.now() + request.timeout_seconds * 1000 : undefined;
  const deadlineSignal = deadline === undefined ? undefined : AbortSignal.timeout(Math.max(1, Math.ceil(deadline - performance.now())));
  const combinedSignal = deadlineSignal ? AbortSignal.any([signal, deadlineSignal]) : signal;
  let slice = 0;
  // The deadline is the cause only when it stopped the work (a cancellation while the client's own
  // signal is still live). Any other error that happened first keeps its own code and message.
  const deadlineCancelled = (cause: Fields | undefined) =>
    deadlineSignal?.aborted === true && !signal.aborted && cause?.code === "task_cancelled";
  try {
    for (let offset = 0; offset < input.length; offset += limit, slice++) {
      if (combinedSignal.aborted) throw cancellationError();
      if (deadline !== undefined && performance.now() >= deadline) throw new Error(JSON.stringify({
        code: "embedding_deadline_exceeded", error: "The embedding request's total deadline expired before the next slice.", execution_outcome: "not_started",
      }));
      const items = input.slice(offset, offset + limit);
      report?.(`embedding items ${offset + 1}-${offset + items.length} of ${input.length}`);
      const next = { ...request, input: items, ...(identity ? {
        model: identity.selected_model, expected_model_digest: identity.model_digest,
        expected_execution_backend: identity.backend, expected_upstream: identity.upstream,
      } : {}), ...(deadline === undefined ? {} : { timeout_seconds: Math.max(1, Math.ceil((deadline - performance.now()) / 1000)) }) };
      const raw = await runManagedTask(target, next, report, combinedSignal, (id) => acceptedJobs.push(id));
      const parsed = object(JSON.parse(raw));
      if (!parsed) throw new Error("embedding slice returned no managed receipt");
      if (typeof parsed.error === "string") throw new Error(JSON.stringify(parsed));
      const response = object(parsed.response);
      // Retain the receipt even if its vectors or identity cannot join the result; never expose
      // a partial collection as a successful embedding space.
      receipts.push({ ...parsed, response: { ...response, embeddings: undefined,
        embeddings_omitted: { count: Array.isArray(response?.embeddings) ? response.embeddings.length : null } } });
      const embeddings = validateEmbeddingVectors(response?.embeddings, items.length, vectors[0]?.length);
      object(receipts[receipts.length - 1]!.response)!.embeddings_omitted = { count: embeddings.length, dimensions: embeddings[0]!.length };
      const actual = sliceIdentity(parsed, embeddings[0]!.length);
      if (identity && canonical(actual) !== canonical(identity)) throw new Error("embedding identity mismatch between slices (model, digest, backend, endpoint, dimensions or effective options changed)");
      identity ??= actual;
      first ??= parsed;
      vectors.push(...embeddings);
    }
  } catch (error) {
    let cause: Fields | undefined;
    try { cause = object(JSON.parse(error instanceof Error ? error.message : String(error))); } catch { /* Plain validation errors remain text. */ }
    throw new Error(JSON.stringify({
      code: deadlineCancelled(cause) ? "embedding_deadline_exceeded" : cause?.code ?? "embedding_slices_invalid",
      error: `Embedding stopped at slice ${slice + 1}; no combined vectors were returned. ${cause?.error ?? (error instanceof Error ? error.message : String(error))}`,
      completed_inputs: vectors.length, total_inputs: input.length, failed_slice: slice + 1,
      embedding_slices: receipts, accepted_job_ids: acceptedJobs, cause,
      retry_safe: vectors.length === 0 && cause?.execution_outcome === "not_started",
    }));
  }
  const metrics = sumFields(receipts, "metrics", ["total_duration_ns", "load_duration_ns", "prompt_tokens", "cached_prompt_tokens",
    "uncached_prompt_tokens", "prompt_duration_ns", "output_tokens", "output_duration_ns"]);
  for (const kind of ["prompt", "output"]) {
    const count = metrics[kind === "prompt" ? "uncached_prompt_tokens" : "output_tokens"];
    const duration = metrics[`${kind}_duration_ns`];
    metrics[`${kind}_tokens_per_second`] = typeof count === "number" && typeof duration === "number" && duration > 0 ? count / duration * 1e9 : null;
  }
  const response = { model: identity?.selected_model, embeddings: vectors,
    ...sumFields(receipts, "response", ["total_duration", "load_duration", "prompt_eval_count", "prompt_eval_duration"]) };
  return JSON.stringify({ ...first, response, metrics, embedding_identity: identity,
    admission: { mode: "sequential_embedding_slices", ...sumFields(receipts, "admission", ["queue_wait_ms", "resource_wait_ms", "transition_wait_ms", "total_wait_ms"]) },
    embedding_requests: receipts.length, embedding_slices: receipts, accepted_job_ids: acceptedJobs });
}

/**
 * Embedding items per job. One request of several hundred chunks crashed nomic-embed-text's runner
 * (`/tokenize: EOF`) on Ollama 0.40.13; slices run one after another on the same warm runner and
 * cost the same. A deferred job can carry 32 vectors of 4096 floats under its 2 MiB.
 */
export const EMBEDDINGS_PER_JOB = 32;

// Match platform::admission::PRIORITY_WEIGHTS and tasks_api::next_batch_task. These are
// dispatch credits only; the Rust core still owns capacity, queues and admission fairness.
const BATCH_PRIORITY_WEIGHTS = { interactive: 3, normal: 2, background: 1 } as const;

/** Independent jobs keep cancellation addressable without depending on a progress token. */
async function runCancellableBatch(
  target: string, tasks: { id: string; independent: boolean; task: Fields }[], maxParallelism: number | undefined,
  signal: AbortSignal, report?: (message: string) => void,
): Promise<string> {
  const parallelism = Math.max(1, Math.min(maxParallelism ?? 8, tasks.length, 64));
  const results: Fields[] = new Array(tasks.length);
  const pending = tasks.map((_, index) => index);
  const priorities = Object.keys(BATCH_PRIORITY_WEIGHTS) as (keyof typeof BATCH_PRIORITY_WEIGHTS)[];
  let credits: Record<keyof typeof BATCH_PRIORITY_WEIGHTS, number> = { ...BATCH_PRIORITY_WEIGHTS };
  const next = (): number => {
    for (let pass = 0; pass < 2; pass++) {
      for (const priority of priorities) {
        if (credits[priority] === 0) continue;
        const position = pending.findIndex((index) => (tasks[index]!.task.priority ?? "normal") === priority);
        if (position !== -1) {
          credits[priority] -= 1;
          return pending.splice(position, 1)[0]!;
        }
      }
      credits = { ...BATCH_PRIORITY_WEIGHTS };
    }
    return pending.shift()!;
  };
  let settled = 0;
  const settle = (id: string, ok: boolean) => {
    settled += 1;
    report?.(`${settled}/${tasks.length} items done (${id} ${ok ? "ok" : "failed"})`);
  };
  await Promise.all(Array.from({ length: parallelism }, async () => {
    while (pending.length > 0) {
      const index = next();
      const item = tasks[index]!;
      const jobIds: string[] = [];
      try {
        if (signal.aborted) throw cancellationError();
        const itemReport = report ? (message: string) => report(`${item.id}: ${message}`) : undefined;
        const response = JSON.parse(await (item.task.task === "embedding" && Array.isArray(item.task.input) && item.task.input.length > EMBEDDINGS_PER_JOB
          ? embedInSlices(target, item.task, item.task.input, itemReport, signal)
          : runManagedTask(target, item.task, itemReport, signal, (id) => jobIds.push(id))));
        if (!object(response)) throw new Error("task returned no managed receipt");
        if (typeof object(response)?.error === "string") throw new Error(JSON.stringify(response));
        if (item.task.task === "embedding") validateEmbeddingVectors(object(response.response)?.embeddings,
          Array.isArray(item.task.input) ? item.task.input.length : 1);
        results[index] = { id: item.id, ok: true, response, ...(jobIds.length ? { accepted_job_ids: jobIds } : {}) };
        settle(item.id, true);
      } catch (error) {
        let body: Fields | undefined;
        try { body = object(JSON.parse(error instanceof Error ? error.message : String(error))); } catch { /* retain plain transport failures */ }
        results[index] = { id: item.id, ok: false, error: body ?? { error: error instanceof Error ? error.message : String(error) },
          ...(jobIds.length ? { accepted_job_ids: jobIds } : {}) };
        settle(item.id, false);
      }
    }
  }));
  return JSON.stringify({ independent_only: true, max_parallelism: parallelism,
    scheduler: { scope: "bounded_client_dispatch_and_global_managed_admission", policy: "weighted_fair_round_robin", weights: BATCH_PRIORITY_WEIGHTS,
      note: "each accepted job remains cancellable; priority is enforced by core admission" }, results });
}

async function runBatch(
  tasks: BatchItem[],
  maxParallelism: number | undefined,
  endpoint: string | undefined,
  extra: ToolExtra,
  defer: boolean,
) {
  const ids = tasks.map((item) => item.id.trim());
  if (ids.some((id) => !id) || new Set(ids).size !== ids.length) return errorResult(new Error("every batch item needs a distinct, non-empty id"));
  tasks = tasks.map((item) => ({ ...item, task: withFiles(item.task, `batch item ${item.id}`).task }));
  for (const item of tasks) {
    const problem = validateTaskPayload(item.task, `batch item ${item.id}`);
    if (problem) return errorResult(new Error(problem));
  }
  const payload = {
    tasks: tasks.map(({ id, independent, task }) => ({
      id,
      independent,
      task: {
        task: task.task === undefined
          ? (task.scopeId === undefined ? "completion" : undefined)
          : task.task,
        objective: task.objective ?? (task.scopeId === undefined ? "balanced" : undefined),
        model: task.model,
        session_id: task.sessionId,
        scope_id: task.scopeId,
        scope_revision: task.scopeRevision,
        context_tokens: task.contextTokens,
        execution_preference: task.executionPreference,
        min_placement_evidence: task.minPlacementEvidence,
        required_capabilities: task.requiredCapabilities,
        priority: task.priority ?? "normal",
        max_wait_seconds: task.maxWaitSeconds,
        timeout_seconds: task.timeoutSeconds,
        stop_on_repetition: task.stopOnRepetition,
        prompt: task.prompt,
        images: task.images,
        messages: taskMessages(task),
        input: task.input,
        state: task.state,
        questions: task.questions,
        tools: task.tools,
        keep_alive: task.keepAlive,
        min_confidence: task.minConfidence,
        request_options: {
          format: task.format,
          think: task.think,
          options: task.options,
          logprobs: task.logprobs,
          top_logprobs: task.topLogprobs,
        },
      },
    })),
  };
  const target = endpoint ?? DEFAULT_SERVE_ENDPOINT;
  if (defer) return withText(parsedResult(await submitDeferredBatch(target, payload.tasks, maxParallelism, extra.signal)), deferText);
  const raw = await runCancellableBatch(target, payload.tasks, maxParallelism, extra.signal, progressSink(extra));
  return withBatchTelemetry(parsedResult(raw));
}

/**
 * `defer` with `tasks`: every item becomes its own deferred job, so no call outlives a client's
 * request timeout. `maxParallelism` bounds only how many submissions are in flight; admission
 * and priority stay with the core. An item whose submission fails is reported in its row and
 * does not stop the others.
 */
async function submitDeferredBatch(
  target: string, tasks: { id: string; independent: boolean; task: Fields }[], maxParallelism: number | undefined, signal: AbortSignal,
): Promise<string> {
  const rows: Fields[] = new Array(tasks.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.max(1, Math.min(maxParallelism ?? 8, tasks.length, 64)) }, async () => {
    while (next < tasks.length) {
      const index = next++;
      const item = tasks[index]!;
      try {
        if (signal.aborted) throw cancellationError();
        const accepted = object(JSON.parse(await runTaskRequest(target, { ...item.task, defer: true })));
        const job = object(accepted?.job);
        if (accepted?.deferred !== true || typeof job?.id !== "string") {
          throw new Error(typeof accepted?.error === "string" ? JSON.stringify(accepted) : "serve returned no deferred job acknowledgement");
        }
        rows[index] = { id: item.id, job };
      } catch (error) {
        let body: Fields | undefined;
        try { body = object(JSON.parse(error instanceof Error ? error.message : String(error))); } catch { /* plain transport failure */ }
        rows[index] = { id: item.id, error: body ?? { error: error instanceof Error ? error.message : String(error) } };
      }
    }
  }));
  return JSON.stringify({ deferred: true, jobs: rows });
}

export function registerRunTaskTool(server: McpServer): void {
  server.registerTool(
    "run_task",
    {
      description:
        "Use when: bounded work for a local model: decision, embedding + `nearest`, vision, tools (returns tool_calls you run), completion batches. " +
        "Do not use when: you must stand behind the judgment, need edits across files, or the work is shorter than describing it. " +
        "`tasks` = up to 64 independent items; per-item fields override top-level defaults; one `model` per batch avoids a cold load per model; `maxParallelism` caps dispatch; `preview:true` routes only. " +
        "MCP clients time out near 60 s: for long work pass `defer:true` (with `tasks`, one job per item) and poll task_jobs get. " +
        "Returns: the answer text (decision: one line per question; nearest: ranked labels); receipts in structuredContent.",
      inputSchema: z.object({
        endpoint: endpointParam,
        task: taskParam.removeDefault().optional(),
        objective: objectiveParam,
        model: z.string().min(1).optional().describe(
          "exact tag; omitted: routing picks by capability only and may choose one that does not fit now, so name the catalog's pick for the job",
        ),
        sessionId: z.string().uuid().optional(),
        scopeId: z.string().uuid().optional(),
        scopeRevision: z.number().int().nonnegative().optional(),
        contextTokens: z.number().int().positive().optional().describe("Input + output window (num_ctx)."),
        executionPreference: executionPreferenceParam,
        minPlacementEvidence: minPlacementEvidenceParam,
        requiredCapabilities: requiredCapabilitiesParam,
        prompt: z.string().min(1).optional(),
        systemPrompt: systemPromptParam,
        images: z
          .array(z.string().min(1))
          .min(1)
          .optional()
          .describe("base64 without data-URI prefix, with `prompt` or a decision `state` (prefer `files`). Needs an explicit vision `model`; text in images: the catalog's ocr pick"),
        files: filesParam.describe(
          "Paths or globs the server reads (a refused path lists the allowed roots); 4 MiB per text file, 8 MiB per image, 32 MiB total. " +
            "embedding: ~1.5 KB chunks, an image or .wav is one item. decision: one text file is the state; images and .wav join images. Other tasks: images join images, text goes under `prompt`.",
        ),
        state: stateParam.describe("decision: what the questions ask about; they may name JSON fields in backticks"),
        questions: questionsParam.describe(
          'decision: choice picks one criteria option; noul is yes/no, answered as the probability of yes (criteria {"false","true"} optional); ' +
            "score gives a level from 2-10 criteria descriptions, lowest first (unreliable for severity).",
        ),
        messages: messagesParam.describe("Caller system prompts; no injected task instructions. Overrides prompt."),
        input: embeddingInputParam,
        tools: localToolsParam.describe(
          "Returns tool_calls for you to run; check them, since small models call a tool when none is needed. With `tasks`: a shared default, an item's own `tools` replaces it",
        ),
        keepAlive: z.string().min(1).optional().describe('"0" unloads now, "-1" pins; omitted: adaptive TTL'),
        format: z
          .union([z.literal("json"), z.record(z.string(), z.unknown())])
          .optional(),
        think: z
          .union([z.boolean(), z.string().min(1)])
          .optional(),
        options: z
          .record(z.string(), z.unknown())
          .optional()
          .describe("num_ctx/num_gpu are routing-owned; num_predict caps output."),
        logprobs: z.boolean().optional(),
        topLogprobs: z.number().int().nonnegative().optional(),
        minConfidence: minConfidenceParam,
        priority: z.enum(["interactive", "normal", "background"]).optional(),
        maxWaitSeconds: z.number().int().positive().optional().describe("capped by the operator's max_queue_wait_seconds (default 120); lowered waits show as wait_limit"),
        timeoutSeconds: z.number().int().positive().optional(),
        stopOnRepetition: z
          .boolean()
          .optional()
          .describe("Stop a repeating loop, keeping its text; default on without num_predict"),
        receipt: z
          .enum(["compact", "full"])
          .optional()
          .describe("compact (default): full receipt at response_file; full: inline"),
        defer: z.boolean().optional(),
        returnEmbeddings: z.boolean().optional(),
        nearest: z
          .object({
            query: z.string().min(1).optional(),
            top: z.number().int().positive().max(100).optional(),
            minScore: z.number().min(-1).max(1).optional(),
            documentPrefix: z.string().optional(),
          })
          .strict()
          .optional()
          .describe(
            "embedding: rank instead of returning vectors (works with `defer`). With `query`: the `top` (default 5, max 100) closest items; " +
              "without: the `top` most similar pairs. Chunks are labelled path:Lstart-Lend: read only that range. documentPrefix is the model's own (see its cookbook).",
          ),
        preview: z
          .boolean()
          .optional()
          .describe("Routing only: payload fields and runtime controls are refused; never executes."),
        tasks: z
          .array(batchItemParam)
          .min(1)
          .max(64)
          .optional()
          .describe("Result text: one `## <id>` section per item."),
        maxParallelism: z.number().int().positive().max(64).optional().describe("Batch mode only"),
      }).strict(),
      outputSchema: taskResultSchema.extend(batchResultSchema.shape),
      annotations: { destructiveHint: false },
    },
    async ({
      endpoint,
      task,
      objective,
      model,
      sessionId,
      scopeId,
      scopeRevision,
      contextTokens,
      executionPreference,
      minPlacementEvidence,
      requiredCapabilities,
      prompt,
      systemPrompt,
      images,
      messages,
      input,
      files,
      state,
      questions,
      tools,
      keepAlive,
      format,
      think,
      options,
      logprobs,
      topLogprobs,
      minConfidence,
      priority,
      maxWaitSeconds,
      timeoutSeconds,
      stopOnRepetition,
      defer,
      returnEmbeddings,
      nearest: nearestRequest,
      preview,
      tasks,
      maxParallelism,
    }, extra: ToolExtra) => {
      try {
        const singleFields = {
          task, objective, model, sessionId, scopeId, scopeRevision, contextTokens, executionPreference,
          minPlacementEvidence, requiredCapabilities, prompt, systemPrompt, images, messages, input, files, state, questions,
          tools, keepAlive, format, think, options, logprobs, topLogprobs, minConfidence, priority, maxWaitSeconds,
          timeoutSeconds, stopOnRepetition, defer, returnEmbeddings, nearest: nearestRequest, preview,
        };
        if (tasks !== undefined) {
          // `defer` is the one single-call field a batch accepts: it submits every item as its own job.
          const stray = Object.entries(singleFields).filter(([name, value]) => value !== undefined && name !== "defer").map(([name]) => name);
          const unsupported = stray.filter((name) => !BATCH_TASK_FIELDS.has(name)).map((name) => `\`${name}\``);
          if (unsupported.length > 0) {
            return errorResult(new Error(`${unsupported.join(", ")} ${unsupported.length === 1 ? "is" : "are"} not supported with \`tasks\`.`));
          }
          const movable = stray.filter((name) => !BATCH_SHARED_FIELDS.has(name)).map((name) => `\`${name}\``);
          if (movable.length > 0) {
            return errorResult(new Error(
              `With \`tasks\`, put ${movable.join(", ")} inside each item's \`task\`; only ${[...BATCH_SHARED_FIELDS].map((name) => `\`${name}\``).join(", ")} are shared defaults.`,
            ));
          }
          // Shared fields are sent once and apply to every item that does not set its own.
          const shared = Object.fromEntries(stray.map((name) => [name, singleFields[name as keyof typeof singleFields]]));
          const items: BatchItem[] = tasks.map((item) => ({ ...item, task: { ...shared, ...withoutUndefined(parseBatchTask(item)) } as BatchItem["task"] }));
          return await runBatch(items, maxParallelism, endpoint, extra, defer === true);
        }
        if (maxParallelism !== undefined) return errorResult(new Error("`maxParallelism` is valid only with `tasks`."));
        const canonicalTask = task ?? "completion";
        if (preview) {
          const executionOnlyFields = ([
            ["scopeId", scopeId],
            ["scopeRevision", scopeRevision],
            ["prompt", prompt],
            ["systemPrompt", systemPrompt],
            ["images", images],
            ["messages", messages],
            ["input", input],
            ["files", files],
            ["state", state],
            ["questions", questions],
            ["nearest", nearestRequest],
            ["tools", tools],
            ["keepAlive", keepAlive],
            ["format", format],
            ["think", think],
            ["options", options],
            ["logprobs", logprobs],
            ["topLogprobs", topLogprobs],
            ["returnEmbeddings", returnEmbeddings],
            ["maxWaitSeconds", maxWaitSeconds],
            ["priority", priority],
            ["timeoutSeconds", timeoutSeconds],
            ["stopOnRepetition", stopOnRepetition],
            ["defer", defer],
          ] satisfies Array<[string, unknown]>)
            .filter(([, value]) => value !== undefined)
            .map(([name]) => `\`${name}\``);
          if (executionOnlyFields.length > 0) {
            // Only a rejected `tools` has a routing-field equivalent to suggest.
            const next = executionOnlyFields.includes("`tools`")
              ? " Use `requiredCapabilities:[\"tools\"]` to preview tool capability, then make a separate execution call with the payload."
              : " Make a separate execution call with the payload.";
            return errorResult(
              new Error(
                "`preview:true` accepts routing fields only; remove execution-only fields " +
                  `${executionOnlyFields.join(", ")}.${next}`,
              ),
            );
          }
        }
        const effectiveRequiredCapabilities =
          tools === undefined
            ? requiredCapabilities
            : withRequiredCapability(requiredCapabilities, "tools");
        // The core enforces minConfidence inside route(), so preview and synchronous execution
        // refuse there, with the core's actionable message, before any generation.
        if (preview) {
          return withText(parsedResult(
            await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
          ), routeText);
        }
        const filled = withFiles({ task, files, prompt, images, input, state, documentPrefix: nearestRequest?.documentPrefix });
        ({ prompt, images, input, state } = filled.task);
        const problem = validateTaskPayload({
          task, scopeId, scopeRevision, prompt, systemPrompt, messages, images, input, tools, format, think, logprobs,
          topLogprobs, options, returnEmbeddings, state, questions, nearest: nearestRequest,
        });
        if (problem) return errorResult(new Error(problem));
        const items = input === undefined ? [] : Array.isArray(input) ? input : [input];
        const labels = nearestRequest === undefined ? undefined : inputLabels(items.length, filled.files);
        // A deferred job is accepted before it routes, so a floor would otherwise surface only as a
        // failed job. Route first (no generation) so the refusal returns synchronously.
        if (minConfidence && defer && scopeId === undefined) {
          const decision = parsedResult(
            await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
          );
          if (!isStructuredSuccess(decision)) return decision;
        }
        const request = {
          task: scopeId !== undefined && task === undefined ? undefined : canonicalTask,
          objective: objective ?? (scopeId === undefined ? "balanced" : undefined),
          model,
          session_id: sessionId,
          scope_id: scopeId,
          scope_revision: scopeRevision,
          context_tokens: contextTokens,
          required_capabilities: effectiveRequiredCapabilities,
          prompt,
          images,
          messages: taskMessages({ systemPrompt, messages, prompt, images }),
          input,
          // The core embeds the query first and ranks every item, so sync and deferred runs agree.
          nearest: nearestRequest === undefined ? undefined : {
            query: nearestRequest.query, top: nearestRequest.top, min_score: nearestRequest.minScore, labels,
          },
          return_embeddings: nearestRequest === undefined ? undefined : returnEmbeddings,
          state,
          questions,
          tools,
          keep_alive: keepAlive,
          min_confidence: minConfidence,
          priority: priority ?? "normal",
          max_wait_seconds: maxWaitSeconds,
          timeout_seconds: timeoutSeconds,
          stop_on_repetition: stopOnRepetition,
          defer: defer ?? false,
          execution_preference: executionPreference,
          min_placement_evidence: minPlacementEvidence,
          request_options: {
            format,
            think,
            options,
            logprobs,
            top_logprobs: topLogprobs,
          },
        };
        const target = endpoint ?? DEFAULT_SERVE_ENDPOINT;
        // A progressToken asks for messages. The cancellation signal independently asks for
        // addressable jobs, including when the client did not subscribe to progress.
        const report = defer ? undefined : progressSink(extra);
        const slicedEmbedding = request.task === "embedding" && Array.isArray(input) && input.length > EMBEDDINGS_PER_JOB && !defer
          && nearestRequest === undefined;
        // A serve this server is still starting reports its phases too.
        if (report) await ensureServe(endpoint, { signal: extra.signal, report });
        // Slices bound retained job results. If the engine returns larger vectors than its job
        // registry can retain, runWithJobProgress returns an explicit failure without replay.
        const result = parsedResult(slicedEmbedding
          ? await embedInSlices(target, request, input as unknown[], report, extra.signal)
          : await runManagedTask(target, request, report, extra.signal));
        if (isStructuredSuccess(result) && result.structuredContent.deferred === true) return withText(result, deferText);
        if (request.task === "embedding" && nearestRequest === undefined && isStructuredSuccess(result)) {
          validateEmbeddingVectors(object(result.structuredContent.response)?.embeddings, Array.isArray(input) ? input.length : 1);
        }
        // Long loads, evictions and waits are worth a notice even to a client that asked for no
        // progress; a memory refusal is a warning that names what holds the memory.
        if (isStructuredSuccess(result)) {
          for (const message of taskNotices(result.structuredContent)) notice(extra, "info", message);
        } else if (result.isError) {
          const text = result.content?.map((part) => ("text" in part ? part.text : "")).join(" ") ?? "";
          if (/resource admission|memory/i.test(text)) notice(extra, "warning", `FreeLlama could not load the model: ${text.slice(0, 400)}`);
        }
        const finished = withTaskTelemetry(result);
        if (filled.files.length > 0 && isStructuredSuccess(finished)) {
          finished.structuredContent.files_read = filled.files;
        }
        if (nearestRequest !== undefined && isStructuredSuccess(finished)) {
          return rankedResult(finished, nearestRequest.query !== undefined, items.length);
        }
        // The vectors stay in the saved response; the inline result and its text omit them.
        if (!returnEmbeddings && isStructuredSuccess(finished)) {
          const trimmed = summarizeEmbeddings(finished.structuredContent);
          if (trimmed) return { ...finished, content: [{ type: "text" as const, text: taskAnswerText(trimmed) ?? "" }] };
        }
        return finished;
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}

/** An embedding result the core ranked by `nearest`: labelled matches inline, vectors only on request. */
function rankedResult(
  finished: { structuredContent: Record<string, unknown>; content?: unknown },
  withQuery: boolean,
  itemCount: number,
) {
  const ranked = finished.structuredContent.nearest as Parameters<typeof nearestText>[0] | undefined;
  if (!ranked) return errorResult(new Error("serve returned no nearest ranking for the embedding task"));
  const header = withQuery ? `closest of ${itemCount} items to the query:` : `most similar pairs among ${itemCount} items:`;
  return { ...finished, content: [{ type: "text" as const, text: `${header}\n${nearestText(ranked)}` }] };
}
