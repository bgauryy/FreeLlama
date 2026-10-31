// Progress for one managed task: run it as a deferred job, report each phase the job passes
// through, and hand back exactly what the synchronous call would have returned.
import type { ProgressSink } from "./progress.js";

type Fields = Record<string, unknown>;

export type JobCalls = {
  /** The task request with `defer:true`. */
  submit: () => Promise<string>;
  get: (id: string) => Promise<string>;
  cancel: (id: string) => Promise<string>;
  /** The unchanged synchronous call, used when the job path cannot carry this task. */
  sync: () => Promise<string>;
};

type Wait = (ms: number, signal?: AbortSignal) => Promise<void>;

/** What a failed call says about itself: its HTTP status when known, and its JSON error body. */
export type CallError = { status?: number; body?: Fields };

export type JobOptions = {
  /** Aborting it cancels the job. */
  signal?: AbortSignal;
  wait?: Wait;
  /** Reads a submit or poll error; defaults to the NAPI client's errors (message = body). */
  describe?: (error: unknown) => CallError;
  /** The error the synchronous call would have thrown, from a failed job's error body and status. */
  failure?: (body: Fields, status: number | undefined) => Error;
  /** The error thrown after `signal` aborts and the job is cancelled. */
  aborted?: (id: string) => Error;
  /** Called once the server holds job `id`, and once it has settled, disappeared or been cancelled. */
  onJob?: (id: string, open: boolean) => void;
};

const TERMINAL = new Set(["completed", "failed", "cancelled", "expired"]);
/** Job-registry limits: the task itself is fine, it just cannot travel as a retained job. */
const SYNC_FALLBACK_CODES = new Set([
  "task_job_input_too_large", "task_job_result_too_large", "task_job_error_too_large", "task_jobs_full",
]);
const FIRST_POLL_MS = 250;
const MAX_POLL_MS = 2_000;
/** Consecutive failed polls before the transport error is reported; any success resets it. */
const MAX_POLL_FAILURES = 5;

const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : undefined;
const number = (value: unknown): number | undefined => (typeof value === "number" && Number.isFinite(value) ? value : undefined);

const gigabytes = (bytes: number) => `${(bytes / 1e9).toFixed(1)} GB`;
export const seconds = (ms: number) => (ms < 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 1000)} s`);

/** Names of models in an eviction list, whether the list holds names or receipts. */
export function modelNames(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item) => {
    if (typeof item === "string") return [item];
    const named = object(item)?.model ?? object(item)?.name;
    return typeof named === "string" ? [named] : [];
  });
}

function evictedSuffix(detail: Fields | undefined): string {
  const evicted = modelNames(detail?.evicted ?? detail?.evicted_idle_models);
  return evicted.length > 0 ? `, evicted ${evicted.join(", ")}` : "";
}

/** One line for the job's current phase, from the job receipt's status, reason and detail. */
export function jobProgressMessage(job: Fields): string {
  const status = typeof job.status === "string" ? job.status : "queued";
  const reason = typeof job.reason === "string" ? job.reason : "";
  const detail = object(job.detail);
  const model = typeof job.selected_model === "string" && job.selected_model ? job.selected_model : undefined;
  switch (status) {
    case "queued":
      return "queued";
    case "waiting_for_admission":
      return reason === "resources_reserved_waiting_for_backend_capacity"
        ? "memory reserved; waiting for backend capacity"
        : "waiting for backend capacity";
    case "waiting_for_resources": {
      const need = number(detail?.required_available_bytes);
      const available = number(detail?.effective_available_bytes);
      if (need === undefined && available === undefined) {
        return (reason === "checking_host_resources" ? "checking host memory" : "waiting for memory") + evictedSuffix(detail);
      }
      const sizes = [need !== undefined ? `need ${gigabytes(need)}` : undefined, available !== undefined ? `${gigabytes(available)} available` : undefined]
        .filter(Boolean).join(", ");
      const pressure = Array.isArray(detail?.pressure_reasons)
        ? detail.pressure_reasons.map((item) => (typeof item === "string" ? item : JSON.stringify(item)))
        : [];
      return `waiting for memory: ${sizes}${pressure.length > 0 ? ` (host: ${pressure.join(", ")})` : ""}${evictedSuffix(detail)}`;
    }
    case "waiting_for_runner":
      return "waiting for runner";
    case "loading": {
      const expected = number(detail?.expected_load_ms);
      return `loading ${model ?? "model"}${expected !== undefined ? ` (~${seconds(expected)})` : ""}${evictedSuffix(detail)}`;
    }
    case "running":
      return model ? `running ${model}` : "running";
    default:
      return status.replace(/_/g, " ");
  }
}

function errorBody(error: unknown): Fields | undefined {
  try {
    return object(JSON.parse(error instanceof Error ? error.message : String(error)));
  } catch {
    return undefined;
  }
}

/** The NAPI client's errors carry the JSON error body as their message, or name a 404. */
function describeNapiError(error: unknown): CallError {
  const message = error instanceof Error ? error.message : String(error);
  const missing = /does not exist or has expired|\bHTTP 404\b|^\s*(?:404\b|Not Found\s*$)/i.test(message);
  return { body: errorBody(error), ...(missing ? { status: 404 } : {}) };
}

/** The submit or poll failed because the job path is missing or cannot hold this task. */
function jobPathUnavailable({ status, body }: CallError): boolean {
  const code = body?.code;
  return (typeof code === "string" && SYNC_FALLBACK_CODES.has(code)) || status === 404;
}

const defaultWait: Wait = (ms, signal) =>
  new Promise((resolve) => {
    if (signal?.aborted) return resolve();
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener("abort", () => {
      clearTimeout(timer);
      resolve();
    }, { once: true });
  });

/**
 * Run one task as a deferred job and report its phases to `report`. Returns the raw result JSON
 * text, or throws the same error body the synchronous call would have thrown. Falls back to the
 * synchronous call when the server has no job route or a job limit refuses the task (the
 * synchronous path keeps its own size limits). Aborting `options.signal` cancels the job.
 */
export async function runWithJobProgress(
  calls: JobCalls,
  report: ProgressSink,
  options: JobOptions = {},
): Promise<string> {
  const { signal, wait = defaultWait, describe = describeNapiError, onJob } = options;
  let raw: string;
  try {
    raw = await calls.submit();
  } catch (error) {
    if (jobPathUnavailable(describe(error))) return calls.sync();
    throw error;
  }
  let submitted: Fields | undefined;
  try {
    submitted = object(JSON.parse(raw));
  } catch {
    return raw;
  }
  let job = object(submitted?.job);
  const id = typeof job?.id === "string" ? job.id : undefined;
  // A server that ignored `defer` already ran the task; its answer is the synchronous result.
  if (submitted?.deferred !== true || !job || !id) return raw;
  onJob?.(id, true);
  let open = true;
  const settled = () => {
    if (open) onJob?.(id, false);
    open = false;
  };
  // Cancellation waits for the worker to release its permits; the caller does not wait for it.
  const cancel = () => {
    void calls.cancel(id).catch(() => undefined).finally(settled);
  };

  let lastMessage = "";
  let lastPhase = "";
  let delay = FIRST_POLL_MS;
  let failures = 0;
  for (;;) {
    const message = jobProgressMessage(job);
    if (message !== lastMessage) {
      report(message);
      lastMessage = message;
    }
    const phase = `${String(job.status)}|${String(job.reason)}`;
    if (phase !== lastPhase) {
      lastPhase = phase;
      delay = FIRST_POLL_MS;
    }
    if (TERMINAL.has(String(job.status))) break;
    await wait(delay, signal);
    if (signal?.aborted) {
      cancel();
      throw options.aborted?.(id) ?? new Error(`run_task was cancelled by the client; deferred job ${id} was cancelled.`);
    }
    delay = Math.min(MAX_POLL_MS, Math.round(delay * 1.5));
    try {
      const polled = object(object(JSON.parse(await calls.get(id)))?.job);
      if (polled) job = polled;
      failures = 0;
    } catch (error) {
      // The job is gone (expired, removed, or the server restarted): nothing will report it.
      if (jobPathUnavailable(describe(error))) {
        settled();
        return calls.sync();
      }
      failures += 1;
      if (failures >= MAX_POLL_FAILURES) {
        // Nobody will follow the job any more; do not leave it running.
        cancel();
        throw error;
      }
    }
  }
  settled();
  if (job.status === "completed") return JSON.stringify(job.result ?? {});
  const failure = { ...(object(job.error) ?? { error: `deferred job ${id} ended ${String(job.status)}` }) };
  if (typeof failure.code === "string" && SYNC_FALLBACK_CODES.has(failure.code)) {
    report("result too large to keep as a job; running synchronously");
    return calls.sync();
  }
  // The job registry adds these to the error body; the synchronous call returns the body alone.
  const status = typeof failure.status === "number" ? failure.status : undefined;
  delete failure.id;
  delete failure.ok;
  if (status !== undefined) delete failure.status;
  throw options.failure?.(failure, status) ?? new Error(JSON.stringify(failure));
}
