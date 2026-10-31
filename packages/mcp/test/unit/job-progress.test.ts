import { describe, expect, it, vi } from "vitest";
import { jobProgressMessage, runWithJobProgress, type JobCalls } from "../../src/job-progress.js";
import { progressSink } from "../../src/progress.js";

const ID = "6f1c1c1e-1111-4111-8111-111111111111";
const receipt = (job: Record<string, unknown>) => JSON.stringify({ job: { id: ID, ...job } });

/** Calls that walk a job through `phases`, one per poll, then return `last` from the final poll. */
function scripted(phases: Record<string, unknown>[], sync = '{"sync":true}') {
  let polls = 0;
  return {
    submit: vi.fn(async () => JSON.stringify({ deferred: true, job: { id: ID, status: "queued", reason: "discovering_model" } })),
    get: vi.fn(async (_id: string) => receipt(phases[Math.min(polls++, phases.length - 1)]!)),
    cancel: vi.fn(async (_id: string) => receipt({ status: "cancelled" })),
    sync: vi.fn(async () => sync),
  } satisfies JobCalls;
}

const noWait = vi.fn(async () => undefined);

describe("job progress messages", () => {
  it("describes each phase from the job receipt", () => {
    expect(jobProgressMessage({ status: "queued" })).toBe("queued");
    expect(jobProgressMessage({ status: "waiting_for_admission", reason: "waiting_for_backend_capacity" })).toBe("waiting for backend capacity");
    expect(jobProgressMessage({
      status: "waiting_for_resources", reason: "waiting_for_safe_memory_and_pressure",
      detail: { required_available_bytes: 9.1e9, effective_available_bytes: 6.4e9, reserve_bytes: 2e9, pressure_reasons: ["active_swapping"] },
    })).toBe("waiting for memory: need 9.1 GB, 6.4 GB available (host: active_swapping)");
    expect(jobProgressMessage({ status: "waiting_for_resources", reason: "checking_host_resources" })).toBe("checking host memory");
    expect(jobProgressMessage({ status: "waiting_for_runner" })).toBe("waiting for runner");
    expect(jobProgressMessage({
      status: "loading", selected_model: "gpt-oss:20b", detail: { expected_load_ms: 8_700, evicted: ["glm-ocr:latest"] },
    })).toBe("loading gpt-oss:20b (~8.7 s), evicted glm-ocr:latest");
    expect(jobProgressMessage({ status: "loading", selected_model: "m" })).toBe("loading m");
    expect(jobProgressMessage({ status: "running", selected_model: "m" })).toBe("running m");
  });
});

describe("runWithJobProgress", () => {
  it("reports each change once and returns the completed result", async () => {
    const calls = scripted([
      { status: "waiting_for_runner", reason: "waiting_for_runner_transition" },
      { status: "waiting_for_runner", reason: "waiting_for_runner_transition" },
      { status: "loading", reason: "forwarding_to_ollama", selected_model: "m" },
      { status: "running", reason: "resident_runner_observed", selected_model: "m" },
      { status: "completed", reason: "completed", selected_model: "m", result: { response: { message: { content: "hi" } } } },
    ]);
    const messages: string[] = [];
    const raw = await runWithJobProgress(calls, (message) => messages.push(message), undefined, noWait);
    expect(JSON.parse(raw)).toEqual({ response: { message: { content: "hi" } } });
    expect(messages).toEqual(["queued", "waiting for runner", "loading m", "running m", "completed"]);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("backs off while the phase stays the same and starts over when it changes", async () => {
    const same = { status: "waiting_for_admission", reason: "waiting_for_backend_capacity" };
    const calls = scripted([same, same, same, same, same, same, same, same, { status: "running" }, { status: "completed", result: {} }]);
    const delays: number[] = [];
    await runWithJobProgress(calls, () => undefined, undefined, async (ms) => { delays.push(ms); });
    expect(delays[0]).toBe(250);
    expect(Math.max(...delays)).toBeLessThanOrEqual(2_000);
    expect(delays.slice(1, 8).every((delay, index) => delay >= delays[index]!)).toBe(true);
    expect(delays.at(-1)).toBe(250);
  });

  it("throws the error body the synchronous call would have thrown", async () => {
    const calls = scripted([{ status: "failed", error: { id: ID, ok: false, status: 409, error: "scope revision changed", code: "scope_conflict" } }]);
    await expect(runWithJobProgress(calls, () => undefined, undefined, noWait))
      .rejects.toThrow(JSON.stringify({ error: "scope revision changed", code: "scope_conflict" }));
  });

  it("falls back to the synchronous call when the result is too large to keep as a job", async () => {
    const calls = scripted([{ status: "failed", error: { id: ID, ok: false, status: 413, code: "task_job_result_too_large", error: "too large" } }]);
    const messages: string[] = [];
    expect(await runWithJobProgress(calls, (message) => messages.push(message), undefined, noWait)).toBe('{"sync":true}');
    expect(calls.sync).toHaveBeenCalledTimes(1);
    expect(messages.at(-1)).toMatch(/running synchronously/);
  });

  it("falls back when jobs are unavailable or the job disappears", async () => {
    for (const message of ['{"error":"deferred input exceeds the 1 MiB retention limit","code":"task_job_input_too_large"}', "HTTP 404 Not Found"]) {
      const calls = scripted([]);
      calls.submit.mockRejectedValueOnce(new Error(message));
      expect(await runWithJobProgress(calls, () => undefined, undefined, noWait)).toBe('{"sync":true}');
    }
    const gone = scripted([]);
    gone.get.mockRejectedValue(new Error('{"error":"job does not exist or has expired"}'));
    expect(await runWithJobProgress(gone, () => undefined, undefined, noWait)).toBe('{"sync":true}');
  });

  it("passes other submit errors through without retrying synchronously", async () => {
    const calls = scripted([]);
    calls.submit.mockRejectedValueOnce(new Error('{"error":"confidence below floor","code":"min_confidence_unmet"}'));
    await expect(runWithJobProgress(calls, () => undefined, undefined, noWait)).rejects.toThrow(/confidence below floor/);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("returns the answer of a server that ignored defer", async () => {
    const calls = scripted([]);
    calls.submit.mockResolvedValueOnce('{"selected_model":"m","response":{}}');
    expect(await runWithJobProgress(calls, () => undefined, undefined, noWait)).toBe('{"selected_model":"m","response":{}}');
    expect(calls.get).not.toHaveBeenCalled();
  });

  it("cancels the job when the client cancels the request", async () => {
    const calls = scripted([{ status: "running" }]);
    const controller = new AbortController();
    const wait = async () => { controller.abort(); };
    await expect(runWithJobProgress(calls, () => undefined, controller.signal, wait)).rejects.toThrow(/cancelled by the client/);
    expect(calls.cancel).toHaveBeenCalledWith(ID);
  });

  it("tolerates a few failed polls and reports a persistent failure", async () => {
    const flaky = scripted([{ status: "completed", result: { ok: 1 } }]);
    flaky.get.mockRejectedValueOnce(new Error("connection reset")).mockRejectedValueOnce(new Error("connection reset"));
    expect(JSON.parse(await runWithJobProgress(flaky, () => undefined, undefined, noWait))).toEqual({ ok: 1 });
    const down = scripted([]);
    down.get.mockRejectedValue(new Error("connection refused"));
    await expect(runWithJobProgress(down, () => undefined, undefined, noWait)).rejects.toThrow("connection refused");
  });
});

describe("progressSink", () => {
  it("is absent without a progressToken and counts up with one", async () => {
    expect(progressSink({ _meta: {}, sendNotification: async () => undefined })).toBeUndefined();
    const sent: unknown[] = [];
    const sink = progressSink({ _meta: { progressToken: "t" }, sendNotification: async (notification) => { sent.push(notification); } })!;
    sink("queued");
    sink("running");
    expect(sent).toEqual([
      { method: "notifications/progress", params: { progressToken: "t", progress: 1, message: "queued" } },
      { method: "notifications/progress", params: { progressToken: "t", progress: 2, message: "running" } },
    ]);
  });
});
