// Managed research turns as deferred jobs: phase messages, results and errors equal to the
// synchronous call, refusal without losing cancellation, and the MCP server's cleanup of open jobs.
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cancelOpenJobs, researchProgressFilter, researchProgressMessage } from "../../src/research-progress.js";
import { runAdapter } from "../../src/research/agent-loop.js";
import { BASH_ADAPTER } from "../../src/research/bash-agent.js";
import { PROGRESS_PREFIX, type ResearchProgressEvent } from "../../src/research/progress-events.js";
import { HTTPError, RequestTimeoutError, chatErrorDetails, retryableChatError } from "../../src/research/transport.js";
import { type HttpOptions } from "../../src/research/transport-http.js";
import { requestJsonAsJob } from "../../src/research/transport-jobs.js";

const ID = "6f1c1c1e-1111-4111-8111-111111111111";
const SERVE = "http://serve.test";
const TASKS = `${SERVE}/_freellama/v1/tasks`;
const JOB = `${SERVE}/_freellama/v1/jobs/${ID}`;
const SYNC_RESULT = {
  selected_model: "gpt-oss:20b",
  route: { selected_model: "gpt-oss:20b" },
  execution: { backend: "metal" },
  response: { message: { content: '{"action":"finish","answer":"src/a.ts"}' }, prompt_eval_count: 12, eval_count: 3 },
};

type Reply = { status?: number; body: unknown } | Error;
type Call = { method: string; url: string; body?: Record<string, unknown> | null; headers: Record<string, string>; signal?: AbortSignal };

/**
 * A fake managed server: `submit` answers the deferred POST, `polls` the GETs in order (the last
 * repeats), `sync` the plain POST. Every request is recorded.
 */
function fakeServe(options: { submit?: Reply; polls?: Reply[]; sync?: Reply }) {
  const calls: Call[] = [];
  const polls = [...(options.polls ?? [])];
  const answer = async (reply: Reply): Promise<Buffer> => {
    if (reply instanceof Error) throw reply;
    const body = Buffer.from(JSON.stringify(reply.body), "utf8");
    const status = reply.status ?? 200;
    if (status >= 300) throw new HTTPError(status, "", body);
    return body;
  };
  const request = vi.fn(async (url: string, http: HttpOptions): Promise<Buffer> => {
    const body = http.body ? JSON.parse(http.body.toString("utf8")) : undefined;
    calls.push({ method: http.method, url, body, headers: http.headers, signal: http.signal });
    if (http.method === "POST" && (url === TASKS || url === `${SERVE}/api/chat`)) {
      return answer(body?.defer === true
        ? options.submit ?? { body: { deferred: true, job: { id: ID, status: "queued", reason: "discovering_model" } } }
        : options.sync ?? { body: SYNC_RESULT });
    }
    if (http.method === "GET" && url === JOB) return answer(polls.length > 1 ? polls.shift()! : polls[0] ?? { body: { job: { id: ID, status: "running" } } });
    if (http.method === "POST" && url === `${JOB}/cancel`) return answer({ body: { job: { id: ID, status: "cancelling" } } });
    throw new Error(`unexpected ${http.method} ${url}`);
  });
  const count = (method: string, url: string, defer?: boolean) =>
    calls.filter((call) => call.method === method && call.url === url && (defer === undefined || (call.body?.defer === true) === defer)).length;
  return {
    request, calls,
    submits: () => count("POST", TASKS, true),
    syncs: () => count("POST", TASKS, false),
    polls: () => count("GET", JOB),
    cancels: () => count("POST", `${JOB}/cancel`),
  };
}

const poll = (job: Record<string, unknown>): Reply => ({ body: { job: { id: ID, ...job } } });
const completed = poll({ status: "completed", reason: "completed", selected_model: "gpt-oss:20b", result: SYNC_RESULT });
const noWait = async () => undefined;
const PAYLOAD = { task: "coding", model: "gpt-oss:20b", messages: [{ role: "user", content: "q" }] };

function run(serve: ReturnType<typeof fakeServe>, extra: { timeoutSeconds?: number; wait?: () => Promise<void> } = {}) {
  const messages: string[] = [];
  const jobs: [string, boolean][] = [];
  const result = requestJsonAsJob(TASKS, PAYLOAD, extra.timeoutSeconds ?? 30, {
    report: (message) => messages.push(message), onJob: (id, open) => jobs.push([id, open]),
    request: serve.request, wait: extra.wait ?? noWait,
  });
  return { result, messages, jobs };
}

beforeEach(() => {
  vi.stubEnv("FREELLAMA_AUTH_TOKEN_FILE", "");
});
afterEach(() => {
  vi.unstubAllEnvs();
});

describe("managed research turns as jobs", () => {
  it("reports memory and load phases and returns exactly the synchronous body", async () => {
    const serve = fakeServe({
      polls: [
        poll({ status: "waiting_for_resources", reason: "waiting_for_safe_memory_and_pressure",
          detail: { required_available_bytes: 9.1e9, effective_available_bytes: 6.4e9, pressure_reasons: ["active_swapping"] } }),
        poll({ status: "loading", reason: "forwarding_to_ollama", selected_model: "gpt-oss:20b", detail: { expected_load_ms: 8_700 } }),
        completed,
      ],
    });
    const { result, messages, jobs } = run(serve);
    expect(await result).toEqual(SYNC_RESULT);
    expect(messages).toEqual([
      "queued",
      "waiting for memory: need 9.1 GB, 6.4 GB available (host: active_swapping)",
      "loading gpt-oss:20b (~8.7 s)",
      "completed",
    ]);
    expect(jobs).toEqual([[ID, true], [ID, false]]);
    // The deferred submit is the synchronous request plus `defer:true`, and it ran once.
    expect(serve.calls[0]!.body).toEqual({ ...PAYLOAD, defer: true });
    expect([serve.submits(), serve.syncs(), serve.cancels()]).toEqual([1, 0, 0]);

    const sync = fakeServe({});
    const plain = await requestJsonAsJob(`${SERVE}/api/chat`, PAYLOAD, 30, { report: () => undefined, request: sync.request });
    expect(sync.calls).toHaveLength(1);
    expect(plain).toEqual(SYNC_RESULT);
  });

  it("sends the adapter's auth header on every job request", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "freellama-job-auth-"));
    try {
      writeFileSync(path.join(dir, "token"), "t".repeat(40), "utf8");
      vi.stubEnv("FREELLAMA_AUTH_TOKEN_FILE", path.join(dir, "token"));
      const serve = fakeServe({ polls: [poll({ status: "running" }), completed] });
      await run(serve).result;
      expect(serve.calls.map((call) => call.headers.authorization)).toEqual(Array(serve.calls.length).fill(`Bearer ${"t".repeat(40)}`));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("throws the synchronous call's HTTP error for a failed job, so busy refusals still retry", async () => {
    const refusal = { error: "backend capacity is full", code: "admission_timeout", retry_after_seconds: 5 };
    const serve = fakeServe({ polls: [poll({ status: "failed", reason: "failed", error: { ...refusal, id: ID, ok: false, status: 503 } })] });
    const error = await run(serve).result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect((error as HTTPError).code).toBe(503);
    expect(JSON.parse((error as HTTPError).body.toString("utf8"))).toEqual(refusal);
    expect(retryableChatError(error)).toBe(true);
    expect(chatErrorDetails(error)).toEqual(chatErrorDetails(new HTTPError(503, "Service Unavailable", Buffer.from(JSON.stringify(refusal)))));
    expect(serve.syncs()).toBe(0);

    // A submit refusal is passed through untouched.
    const busy = fakeServe({ submit: { status: 503, body: refusal } });
    const thrown = await run(busy).result.catch((failure: unknown) => failure);
    expect([(thrown as HTTPError).code, busy.syncs(), busy.polls()]).toEqual([503, 0, 0]);
  });

  it.each(["submission", "accepted-job"])("never retries an explicit unsafe 503 from %s", async (phase) => {
    const body = { error: "execution outcome unknown", code: "admission_timeout", retry_safe: false, execution_outcome: "unknown" };
    const serve = fakeServe(phase === "submission"
      ? { submit: { status: 503, body } }
      : { polls: [poll({ status: "failed", error: { ...body, status: 503 } })] });
    const error = await run(serve).result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect(retryableChatError(error)).toBe(false);
    expect(chatErrorDetails(error)[1]).toMatchObject({ receipt: { retry_safe: false, execution_outcome: "unknown" } });
    expect([serve.submits(), serve.syncs()]).toEqual([1, 0]);
  });

  it("marks an untyped submission 503 unknown instead of treating it as a safe overload refusal", async () => {
    const serve = fakeServe({ submit: { status: 503, body: { error: "proxy failed after forwarding" } } });
    const error = await run(serve).result.catch((failure: unknown) => failure);
    expect(retryableChatError(error)).toBe(false);
    expect(chatErrorDetails(error)[1]).toMatchObject({ receipt: { retry_safe: false, execution_outcome: "unknown" } });
  });

  it.each([
    ["the job registry is full", { submit: { status: 429, body: { error: "deferred task registry is full", code: "task_jobs_full" } } }],
    ["the input is too large to retain", { submit: { status: 413, body: { error: "too large", code: "task_job_input_too_large" } } }],
  ] as const)("preserves cancellability by refusing before execution when %s", async (_name, setup) => {
    const serve = fakeServe(setup);
    const error = await run(serve).result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect((error as HTTPError).code).toBe(409);
    expect(chatErrorDetails(error)[1]).toMatchObject({ receipt: {
      code: "cancellable_task_unavailable", execution_outcome: "not_started", retry_safe: true,
    } });
    expect([serve.submits(), serve.syncs(), serve.polls(), serve.cancels()]).toEqual([1, 0, 0, 0]);
  });

  it("does not interpret a submit 404 as proof that no work ran", async () => {
    const serve = fakeServe({ submit: { status: 404, body: { error: "Not Found" } } });
    const error = await run(serve).result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect((error as HTTPError).code).toBe(404);
    expect(retryableChatError(error)).toBe(false);
    expect([serve.submits(), serve.syncs(), serve.polls()]).toEqual([1, 0, 0]);
  });

  it.each(["task_job_result_too_large", "task_job_error_too_large"])("does not replay the turn when the job ends %s", async (code) => {
    const serve = fakeServe({ polls: [poll({ status: "failed", error: { code, error: "too large", ...(code.includes("result") ? { id: ID, ok: false, status: 413 } : {}) } })] });
    const { result, messages, jobs } = run(serve);
    const error = await result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect(retryableChatError(error)).toBe(false);
    const body = JSON.parse((error as HTTPError).body.toString("utf8"));
    expect(body).toMatchObject({ code, job_id: ID, result_recoverable: false, retry_safe: false });
    expect(body.error).toMatch(/not run again.*not retained/);
    expect(messages.at(-1)).toBe("failed");
    expect([serve.submits(), serve.syncs(), serve.cancels()]).toEqual([1, 0, 0]);
    expect(jobs).toEqual([[ID, true], [ID, false]]);
  });

  it("preserves the missing job identity without replaying its unknown execution", async () => {
    const serve = fakeServe({ polls: [poll({ status: "running" }), { status: 404, body: { error: "job does not exist or has expired" } }] });
    const { result, jobs } = run(serve);
    const error = await result.catch((failure: unknown) => failure);
    expect(error).toBeInstanceOf(HTTPError);
    expect((error as HTTPError).code).toBe(404);
    expect(retryableChatError(error)).toBe(false);
    expect(JSON.parse((error as HTTPError).body.toString("utf8"))).toMatchObject({
      code: "task_job_unavailable", job_id: ID, last_observed_status: "running", execution_outcome: "unknown",
      result_recoverable: false, retry_safe: false,
    });
    expect([serve.submits(), serve.syncs(), serve.cancels()]).toEqual([1, 0, 0]);
    expect(jobs).toEqual([[ID, true], [ID, false]]);
  });

  it("requests cancellation on the turn deadline and keeps an unconfirmed job open", async () => {
    const serve = fakeServe({ polls: [poll({ status: "loading", selected_model: "gpt-oss:20b" })] });
    const { result, jobs } = run(serve, { timeoutSeconds: 0.05, wait: () => new Promise((resolve) => setTimeout(resolve, 20)) });
    await expect(result).rejects.toBeInstanceOf(RequestTimeoutError);
    // This server acknowledges only `cancelling`, not a terminal state. The outer adapter
    // cleanup must retain the handle so it can retry cancellation after the subprocess exits.
    expect(jobs).toEqual([[ID, true]]);
    expect([serve.cancels(), serve.syncs()]).toEqual([1, 0]);
    expect(serve.calls.filter((call) => call.method === "GET").every((call) => call.signal !== undefined)).toBe(true);
  });

  it("cancels the job when polling keeps failing", async () => {
    const serve = fakeServe({ polls: [new Error("socket hang up")] });
    await expect(run(serve).result).rejects.toThrow("socket hang up");
    expect([serve.cancels(), serve.syncs()]).toEqual([1, 0]);
  });
});

describe("research loop managed lifecycle", () => {
  const roots: string[] = [];
  afterEach(() => {
    for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
  });

  it.each([true, false])("tracks jobs and preserves receipts with progress enabled=%s", async (progressEnabled) => {
    const root = mkdtempSync(path.join(tmpdir(), "freellama-jobs-loop-"));
    roots.push(root);
    writeFileSync(path.join(root, "prompt.txt"), "Where is foo?", "utf8");
    // The answer cites src/a.ts; it exists, so the answer passes the cited-path check.
    mkdirSync(path.join(root, "src"), { recursive: true });
    writeFileSync(path.join(root, "src", "a.ts"), "foo", "utf8");
    const serve = fakeServe({
      polls: [poll({ status: "loading", reason: "forwarding_to_ollama", selected_model: "gpt-oss:20b", detail: { expected_load_ms: 8_700 } }), completed],
    });
    const events: ResearchProgressEvent[] = [];
    const env = {
      FREELLAMA_TARGET_MODEL: "gpt-oss:20b", FREELLAMA_BENCH_WORKSPACE: root, FREELLAMA_BENCH_PROMPT: path.join(root, "prompt.txt"),
      FREELLAMA_AGENT_RESULT: path.join(root, "result.json"), FREELLAMA_AGENT_MAX_TURNS: "8", FREELLAMA_AGENT_MANAGED_ENDPOINT: SERVE,
    };
    const code = await runAdapter(BASH_ADAPTER, {
      env, httpRequest: serve.request, print: () => undefined,
      ...(progressEnabled ? { progress: (event: ResearchProgressEvent) => events.push(event) } : {}),
    });
    expect(code).toBe(0);
    expect(events.map((event) => researchProgressMessage(event, root)).filter(Boolean)).toEqual(progressEnabled ? [
      "turn 1/8: model thinking", "turn 1/8: queued", "turn 1/8: loading gpt-oss:20b (~8.7 s)", "turn 1/8: completed", "finishing answer",
    ] : []);
    expect(events.filter((event) => event.event === "job_open" || event.event === "job_closed"))
      .toEqual(progressEnabled ? [{ event: "job_open", id: ID }, { event: "job_closed", id: ID }] : []);
    expect([serve.submits(), serve.syncs()]).toEqual([1, 0]);
    const result = JSON.parse(readFileSync(path.join(root, "result.json"), "utf8"));
    expect(result.final_answer).toBe("src/a.ts");
    expect(result.model_metadata.execution_receipts).toEqual([{ model: "gpt-oss:20b", execution: { backend: "metal" }, admission: null, feedback: null }]);
    expect(result.usage).toMatchObject({ input_tokens: 12, output_tokens: 3 });
    expect(researchProgressMessage({ event: "job_phase", maxTurns: 8, message: "queued" }, root)).toBe("final answer: queued");
  });
});

describe("open jobs of an exited adapter", () => {
  it("tracks job lines and cancels the ones still open", async () => {
    const reported: string[] = [];
    const open = new Set<string>();
    const filter = researchProgressFilter((message) => reported.push(message), "/w", open);
    const line = (event: Record<string, unknown>) => `${PROGRESS_PREFIX}${JSON.stringify(event)}\n`;
    filter.push(Buffer.from(line({ event: "job_open", id: "a" }) + line({ event: "job_open", id: "b" }) + line({ event: "job_closed", id: "a" })
      + line({ event: "job_phase", turn: 2, maxTurns: 8, message: "loading m (~8.7 s)" })));
    expect([...open]).toEqual(["b"]);
    expect(reported).toEqual(["turn 2/8: loading m (~8.7 s)"]);
    const cancel = vi.fn(async (_id: string) => {
      throw new Error("server gone");
    });
    cancelOpenJobs(open, cancel);
    expect(cancel.mock.calls).toEqual([["b"]]);
    expect(open.size).toBe(0);
  });
});
