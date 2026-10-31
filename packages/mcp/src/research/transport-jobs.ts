// A managed research turn as a deferred job: the same POST /_freellama/v1/tasks with `defer:true`,
// polled so the wait for admission, memory or a model load is reported while it happens. The
// phases, fallbacks and polling are run_task's (src/job-progress.ts); this module only maps them
// onto the adapter's HTTP client, so a turn returns the synchronous call's body and throws its
// `HTTPError`, `URLError` or `RequestTimeoutError`.
import { STATUS_CODES } from "node:http";
import { runWithJobProgress, type CallError, type JobOptions } from "../job-progress.js";
import { isRecord } from "./canonical-json.js";
import { HTTPError, RECEIPT_BODY_LIMIT, RequestTimeoutError, httpRequest, parseJsonBody, requestHeaders } from "./transport-http.js";
import { decodeUtf8 } from "./text.js";

const TASKS_SUFFIX = "/_freellama/v1/tasks";

export interface JobRequestOptions {
  /** Receives one line per job phase. */
  report: (message: string) => void;
  /** Called once the server holds job `id`, and once it has settled, disappeared or been cancelled. */
  onJob?: (id: string, open: boolean) => void;
  request?: typeof httpRequest;
  /** Replaces the poll back-off wait (tests). */
  wait?: JobOptions["wait"];
}

/** The status and JSON body of an adapter HTTP error; other failures describe nothing. */
function describeHttpError(error: unknown): CallError {
  if (!(error instanceof HTTPError)) return {};
  try {
    const body = parseJsonBody(error.body.subarray(0, RECEIPT_BODY_LIMIT), "error body");
    return { status: error.code, ...(isRecord(body) ? { body } : {}) };
  } catch {
    return { status: error.code };
  }
}

/** The HTTP error the synchronous call answers with, rebuilt from a failed job's error body. */
function syncError(body: Record<string, unknown>, status: number | undefined): HTTPError {
  const code = status ?? 500;
  return new HTTPError(code, STATUS_CODES[code] ?? "", Buffer.from(JSON.stringify(body), "utf8"));
}

/**
 * POST one managed chat request as a deferred job and decode the JSON the synchronous call would
 * have returned. `timeoutSeconds` bounds the whole turn, as the synchronous call's idle socket
 * does: when it passes the job is cancelled and the turn fails with {@link RequestTimeoutError}.
 * Without a managed tasks URL this is the synchronous call.
 */
export async function requestJsonAsJob(
  url: string,
  payload: Record<string, unknown>,
  timeoutSeconds: number,
  options: JobRequestOptions,
): Promise<unknown> {
  const request = options.request ?? httpRequest;
  const headers = requestHeaders();
  const call = async (method: "GET" | "POST", target: string, body?: unknown, signal?: AbortSignal) => {
    const sent = body === undefined ? undefined : Buffer.from(JSON.stringify(body), "utf8");
    const response = await request(target, { method, headers, body: sent, timeoutSeconds, signal });
    return decodeUtf8(response, "managed task response");
  };
  const sync = () => call("POST", url, payload);
  if (!url.endsWith(TASKS_SUFFIX)) return parseJsonBody(Buffer.from(await sync(), "utf8"), "chat response");
  const jobs = `${url.slice(0, -TASKS_SUFFIX.length)}/_freellama/v1/jobs`;
  const deadline = new AbortController();
  const timer = setTimeout(() => deadline.abort(), Math.max(timeoutSeconds * 1000, 1));
  try {
    const raw = await runWithJobProgress({
      submit: () => call("POST", url, { ...payload, defer: true }),
      get: (id) => call("GET", `${jobs}/${encodeURIComponent(id)}`, undefined, deadline.signal),
      cancel: (id) => call("POST", `${jobs}/${encodeURIComponent(id)}/cancel`, null),
      sync,
    }, options.report, {
      signal: deadline.signal,
      wait: options.wait,
      describe: describeHttpError,
      failure: syncError,
      aborted: () => new RequestTimeoutError(),
      onJob: options.onJob,
    });
    return parseJsonBody(Buffer.from(raw, "utf8"), "chat response");
  } finally {
    clearTimeout(timer);
  }
}
