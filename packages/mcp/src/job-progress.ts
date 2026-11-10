// Progress for one managed task: run it as a deferred job, report each phase the job passes
// through, and hand back exactly what the synchronous call would have returned.
import type { ProgressSink } from "./progress.js";

type Fields = Record<string, unknown>;

export type JobCalls = {
  /** The task request with `defer:true`. */
  submit: () => Promise<string>;
  /** `waitMs`: ask serve to hold the read until the job changes (a long poll); older serves ignore it. */
  get: (id: string, waitMs?: number) => Promise<string>;
  cancel: (id: string) => Promise<string>;
  /**
   * The unchanged synchronous call, used only when submission was refused before execution;
   * `refusal` is serve's error body for that refusal.
   */
  sync: (refusal?: Fields) => Promise<string>;
};

type Wait = (ms: number, signal?: AbortSignal) => Promise<void>;

/** What a failed call says about itself: its HTTP status when known, and its JSON error body. */
export type CallError = { status?: number; body?: Fields };

/**
 * What cancelling an accepted job achieved: `acknowledged` when serve answered with a terminal
 * job state, `requested` when serve accepted the request but had not finished the job, `unknown`
 * when no usable acknowledgement arrived after every attempt (or they outlasted the report cap), so the
 * job may still be running. None of them proves the upstream model stopped.
 */
export type CancellationState = "acknowledged" | "requested" | "unknown";

export type JobOptions = {
  /** Aborting it cancels the job. */
  signal?: AbortSignal;
  wait?: Wait;
  /** Reads a submit or poll error; defaults to the NAPI client's errors (message = body). */
  describe?: (error: unknown) => CallError;
  /** The error the synchronous call would have thrown, from a failed job's error body and status. */
  failure?: (body: Fields, status: number | undefined) => Error;
  /** The error thrown on abort; `cancellation` is what the cancel request achieved, none of which confirms an upstream stop. */
  aborted?: (id?: string, cancellation?: CancellationState) => Error;
  /**
   * The error thrown when submission failed for a reason other than a pre-execution refusal and
   * no job handle came back, so whether the task ran is unknown. Defaults to the error's body
   * with `execution_outcome:"unknown"` and `retry_safe:false` added.
   */
  unconfirmed?: (error: unknown, body: Fields | undefined) => Error;
  /** Called once the server holds job `id`, and once it has settled, disappeared or been cancelled. */
  onJob?: (id: string, open: boolean) => void;
};

const TERMINAL = new Set(["completed", "failed", "cancelled", "expired"]);
/** Submission refusals that establish no worker was started. */
const SUBMIT_FALLBACK_CODES = new Set(["task_job_input_too_large", "task_jobs_full", "task_jobs_input_budget"]);
/** The worker already ran, but the registry could not retain its result or error. */
const RETENTION_FAILURE_CODES = new Set(["task_job_result_too_large", "task_job_error_too_large"]);
/**
 * The first poll comes soon and the interval doubles up to MAX_POLL_MS: a short task (most local
 * tasks finish in 100-300 ms on a warm runner) is reported within a few tens of milliseconds of
 * finishing, not at the first 250 ms tick or the next one at 625 ms (measured), while a long
 * task still settles to one cheap local poll every two seconds.
 */
const FIRST_POLL_MS = 25;
/**
 * A phase that lasts this long is reported again with its elapsed time, so a cold load or a long
 * memory wait still sends progress: clients that reset their request timeout on progress keep
 * the call alive, and a person sees it is not stuck.
 */
const HEARTBEAT_MS = 10_000;
const MAX_POLL_MS = 2_000;
/**
 * How long one read may wait for the job to change on a serve that long-polls. A change answers
 * at once; the cap keeps a cancellation (checked between reads) responsive.
 */
const LONG_POLL_MS = 1_000;
/** Consecutive failed polls before the transport error is reported; any success resets it. */
const MAX_POLL_FAILURES = 5;
/** Cancel requests sent before the cancellation is reported as unknown. */
const CANCEL_ATTEMPTS = 3;
const CANCEL_BACKOFF_MS = 250;
/** The longest an abort waits for its cancel attempts before reporting what is known. */
const CANCEL_REPORT_MS = 5_000;

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
/** "; largest memory users: rust-analyzer 2.0 GB, Google Chrome 0.5 GB" when serve named them. */
function holdersSuffix(detail: Record<string, unknown> | undefined): string {
  const holders = Array.isArray(detail?.memory_holders) ? detail.memory_holders : [];
  const named = holders
    .map((holder) => holder as { name?: unknown; resident_bytes?: unknown; processes?: unknown })
    .filter((holder) => typeof holder.name === "string" && typeof holder.resident_bytes === "number")
    .slice(0, 3)
    .map((holder) => {
      const count = typeof holder.processes === "number" && holder.processes > 1 ? ` ×${holder.processes}` : "";
      return `${holder.name as string}${count} ${gigabytes(holder.resident_bytes as number)}`;
    });
  return named.length > 0 ? `; largest memory users: ${named.join(", ")}` : "";
}

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
      return `waiting for memory: ${sizes}${pressure.length > 0 ? ` (host: ${pressure.join(", ")})` : ""}${holdersSuffix(detail)}${evictedSuffix(detail)}`;
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

/**
 * A submit failure without a job handle. A typed serve refusal (a body with `code`) is kept
 * whole: the submit defers, so an HTTP refusal created no job and ran nothing. Without such an
 * answer (transport failure, unparseable body) the task may have run, so it is marked unknown.
 */
function unconfirmedSubmission(error: unknown, body: Fields | undefined): Error {
  const message = error instanceof Error ? error.message : String(error);
  if (body && (typeof body.code === "string" || "execution_outcome" in body || "retry_safe" in body)) {
    return error instanceof Error ? error : new Error(message);
  }
  return new Error(JSON.stringify({
    ...body,
    error: typeof body?.error === "string" ? body.error : message,
    execution_outcome: "unknown",
    retry_safe: false,
  }));
}

/** Only a refused submission can fall back; a missing accepted job has an unknown outcome. */
function submissionRefused({ body }: CallError): boolean {
  const code = body?.code;
  // An arbitrary 404 (including one from an older endpoint) does not prove that no work ran.
  return typeof code === "string" && SUBMIT_FALLBACK_CODES.has(code);
}

const defaultWait: Wait = (ms, signal) =>
  new Promise((resolve) => {
    if (signal?.aborted) return resolve();
    const done = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    signal?.addEventListener("abort", done, { once: true });
  });

/**
 * Run one task as a deferred job and report its phases to `report`. Returns the raw result JSON
 * text, or throws the task's error body. Falls back to the synchronous call only when submission
 * was refused before execution. Once a job is accepted, a lost record or retention failure must
 * never replay its work. Aborting `options.signal` requests cancellation of the job.
 */
export async function runWithJobProgress(
  calls: JobCalls,
  report: ProgressSink,
  options: JobOptions = {},
): Promise<string> {
  const { signal, wait = defaultWait, describe = describeNapiError, onJob } = options;
  const aborted = (id?: string, cancellation?: CancellationState) => options.aborted?.(id, cancellation) ?? new Error(id
    ? cancellation === "unknown"
      ? `run_task was cancelled by the client; cancellation acknowledgement is unknown for deferred job ${id}, which may still be running. ` +
        "Upstream stop is not confirmed."
      : `run_task was cancelled by the client; cancellation was requested for deferred job ${id}` +
        `${cancellation === "acknowledged" ? " and acknowledged by serve" : ""}. Upstream stop is not confirmed.`
    : "run_task was cancelled by the client before a job handle was received; execution outcome is unknown. The task was not replayed.");
  if (signal?.aborted) throw options.aborted?.() ?? new Error("run_task was cancelled by the client before submission; no task was submitted.");
  let raw: string;
  try {
    raw = await calls.submit();
  } catch (error) {
    if (signal?.aborted) throw aborted();
    const described = describe(error);
    if (submissionRefused(described)) return calls.sync(described.body);
    // No job handle came back, so the task may have run. Say so, and never replay it.
    throw options.unconfirmed?.(error, described.body) ?? unconfirmedSubmission(error, described.body);
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
  // Cancellation is bounded: a failed request is retried with backoff, then the job stays open
  // for any outer cleanup (the research delegate cancels jobs left open). The caller does not
  // wait for it, except to report its state when the run ends.
  let cancellation: Promise<CancellationState> | undefined;
  const requestCancellation = async (): Promise<CancellationState> => {
    for (let attempt = 1; ; attempt++) {
      try {
        const reply = object(JSON.parse(await calls.cancel(id)));
        const status = object(reply?.job)?.status ?? reply?.status;
        if (!TERMINAL.has(String(status))) return "requested";
        settled();
        return "acknowledged";
      } catch (error) {
        // A job serve no longer holds cannot be cancelled again; nothing is left to retry.
        if (describe(error).status === 404) {
          settled();
          return "unknown";
        }
        if (attempt >= CANCEL_ATTEMPTS) return "unknown";
        await wait(CANCEL_BACKOFF_MS * 2 ** (attempt - 1));
      }
    }
  };
  const cancel = () => {
    cancellation ??= requestCancellation();
  };
  /** The cancellation's state once its attempts finish, or `unknown` if they outlast the report cap. */
  const cancellationState = async (): Promise<CancellationState | undefined> => {
    if (!cancellation) return undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const capped = new Promise<CancellationState>((resolve) => { timer = setTimeout(() => resolve("unknown"), CANCEL_REPORT_MS); });
    try {
      return await Promise.race([cancellation, capped]);
    } finally {
      clearTimeout(timer);
    }
  };
  signal?.addEventListener("abort", cancel, { once: true });

  try {
    let lastMessage = "";
    let lastPhase = "";
    let lastReportAt = Date.now();
    let phaseStartedAt = lastReportAt;
    let delay = FIRST_POLL_MS;
    let longPoll = false;
    let failures = 0;
    for (;;) {
      if (signal?.aborted) {
        cancel();
        throw aborted(id, await cancellationState());
      }
      const now = Date.now();
      const phase = `${String(job.status)}|${String(job.reason)}`;
      if (phase !== lastPhase) {
        lastPhase = phase;
        phaseStartedAt = now;
        delay = FIRST_POLL_MS;
      }
      const message = jobProgressMessage(job);
      if (message !== lastMessage) {
        report(message);
        lastMessage = message;
        lastReportAt = now;
      } else if (now - lastReportAt >= HEARTBEAT_MS && !TERMINAL.has(String(job.status))) {
        report(`${message} (${seconds(now - phaseStartedAt)})`);
        lastReportAt = now;
      }
      if (TERMINAL.has(String(job.status))) break;
      // A long-polling serve answers when the job changes, so there is nothing to sleep for.
      if (!longPoll) await wait(delay, signal);
      if (signal?.aborted) {
        cancel();
        throw aborted(id, await cancellationState());
      }
      delay = Math.min(MAX_POLL_MS, delay * 2);
      try {
        const reply = object(JSON.parse(await calls.get(id, LONG_POLL_MS)));
        if (reply?.long_poll === true) longPoll = true;
        const polled = object(reply?.job);
        if (polled) job = polled;
        failures = 0;
      } catch (error) {
        // A lost record does not establish whether inference ran. Preserve its identity and fail
        // rather than repeating work that may have completed before removal, expiry or restart.
        if (describe(error).status === 404) {
          settled();
          const failure = {
            code: "task_job_unavailable",
            error: `Deferred job ${id} is no longer available; its execution outcome is unknown. ` +
              "The task was not run again. Its result cannot be recovered from the missing job record; " +
              "verify the original execution before submitting new work.",
            job_id: id,
            last_observed_status: job.status,
            execution_outcome: "unknown",
            result_recoverable: false,
            retry_safe: false,
          };
          throw options.failure?.(failure, 404) ?? new Error(JSON.stringify(failure));
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
    if (typeof failure.code === "string" && RETENTION_FAILURE_CODES.has(failure.code)) {
      const lost = failure.code === "task_job_result_too_large" ? "result" : "error";
      failure.error = `${String(failure.error)} Deferred job ${id} was not run again. ` +
        `Its original ${lost} was not retained and cannot be recovered from this job; ` +
        "resubmission would execute new work.";
      failure.job_id = id;
      failure.result_recoverable = false;
      failure.retry_safe = false;
    }
    // The job registry adds these to the error body; the synchronous call returns the body alone.
    const status = typeof failure.status === "number" ? failure.status : undefined;
    delete failure.id;
    delete failure.ok;
    if (status !== undefined) delete failure.status;
    throw options.failure?.(failure, status) ?? new Error(JSON.stringify(failure));
  } finally {
    signal?.removeEventListener("abort", cancel);
  }
}
