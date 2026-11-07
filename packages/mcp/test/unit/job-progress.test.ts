import { getEventListeners } from "node:events";
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
    expect(jobProgressMessage({
      status: "waiting_for_resources", reason: "waiting_for_safe_memory_and_pressure",
      detail: { required_available_bytes: 9.1e9, effective_available_bytes: 6.4e9, pressure_reasons: ["low_available_memory"],
        memory_holders: [{ name: "rust-analyzer", pid: 1, resident_bytes: 2.0e9 }, { name: "Google Chrome", pid: 2, resident_bytes: 0.5e9 }] },
    })).toBe("waiting for memory: need 9.1 GB, 6.4 GB available (host: low_available_memory); largest memory users: rust-analyzer 2.0 GB, Google Chrome 0.5 GB");
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
    const raw = await runWithJobProgress(calls, (message) => messages.push(message), { wait: noWait });
    expect(JSON.parse(raw)).toEqual({ response: { message: { content: "hi" } } });
    expect(messages).toEqual(["queued", "waiting for runner", "loading m", "running m", "completed"]);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("backs off while the phase stays the same and starts over when it changes", async () => {
    const same = { status: "waiting_for_admission", reason: "waiting_for_backend_capacity" };
    const calls = scripted([same, same, same, same, same, same, same, same, { status: "running" }, { status: "completed", result: {} }]);
    const delays: number[] = [];
    await runWithJobProgress(calls, () => undefined, { wait: async (ms) => { delays.push(ms); } });
    expect(delays[0]).toBe(250);
    expect(Math.max(...delays)).toBeLessThanOrEqual(2_000);
    expect(delays.slice(1, 8).every((delay, index) => delay >= delays[index]!)).toBe(true);
    expect(delays.at(-1)).toBe(250);
  });

  it("throws the error body the synchronous call would have thrown", async () => {
    const calls = scripted([{ status: "failed", error: { id: ID, ok: false, status: 409, error: "scope revision changed", code: "scope_conflict" } }]);
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait }))
      .rejects.toThrow(JSON.stringify({ error: "scope revision changed", code: "scope_conflict" }));
  });

  it.each(["task_job_result_too_large", "task_job_error_too_large"])("does not replay accepted work after %s", async (code) => {
    const calls = scripted([{ status: "failed", error: { id: ID, ok: false, status: 413, code, error: "too large" } }]);
    const messages: string[] = [];
    const onJob = vi.fn();
    const error = await runWithJobProgress(calls, (message) => messages.push(message), { wait: noWait, onJob })
      .catch((failure: Error) => failure);
    expect(error).toBeInstanceOf(Error);
    const failure = JSON.parse((error as Error).message);
    expect(failure).toMatchObject({ code, job_id: ID, result_recoverable: false, retry_safe: false });
    expect(failure.error).toMatch(/not run again.*not retained.*cannot be recovered/);
    expect(failure).not.toHaveProperty("status");
    expect(calls.submit).toHaveBeenCalledTimes(1);
    expect(calls.sync).not.toHaveBeenCalled();
    expect(calls.cancel).not.toHaveBeenCalled();
    expect(messages).toEqual(["queued", "failed"]);
    expect(onJob.mock.calls).toEqual([[ID, true], [ID, false]]);
  });

  it("falls back only when submission was refused before execution", async () => {
    for (const message of [
      '{"error":"deferred input exceeds the 1 MiB retention limit","code":"task_job_input_too_large"}',
      '{"error":"deferred task registry is full","code":"task_jobs_full"}',
    ]) {
      const calls = scripted([]);
      calls.submit.mockRejectedValueOnce(new Error(message));
      expect(await runWithJobProgress(calls, () => undefined, { wait: noWait })).toBe('{"sync":true}');
      expect(calls.sync).toHaveBeenCalledTimes(1);
      expect(calls.get).not.toHaveBeenCalled();
    }
  });

  it("keeps the accepted job identity and unknown outcome when its record disappears", async () => {
    const gone = scripted([]);
    gone.get.mockResolvedValueOnce(receipt({ status: "running" }))
      .mockRejectedValue(new Error('{"error":"job does not exist or has expired"}'));
    const onJob = vi.fn();
    const error = await runWithJobProgress(gone, () => undefined, { wait: noWait, onJob })
      .catch((failure: Error) => failure);
    expect(error).toBeInstanceOf(Error);
    const failure = JSON.parse((error as Error).message);
    expect(failure).toMatchObject({
      code: "task_job_unavailable", job_id: ID, last_observed_status: "running",
      execution_outcome: "unknown", result_recoverable: false, retry_safe: false,
    });
    expect(failure.error).toMatch(/not run again.*cannot be recovered/);
    expect(gone.submit).toHaveBeenCalledTimes(1);
    expect(gone.get).toHaveBeenCalledTimes(2);
    expect(gone.sync).not.toHaveBeenCalled();
    expect(gone.cancel).not.toHaveBeenCalled();
    expect(onJob.mock.calls).toEqual([[ID, true], [ID, false]]);
  });

  it.each(["task_job_input_too_large", "task_jobs_full"])("never replays a %s failure after acceptance", async (code) => {
    const calls = scripted([{ status: "failed", error: { code, error: "late refusal" } }]);
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait })).rejects.toThrow(/late refusal/);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it.each([
    ["min_confidence_unmet", "confidence below floor"],
    ["task_job_result_too_large", "result too large"],
    ["task_job_error_too_large", "error too large"],
  ])("passes uncertain or execution-related submit failure %s through without replay", async (code, error) => {
    const calls = scripted([]);
    calls.submit.mockRejectedValueOnce(new Error(JSON.stringify({ code, error })));
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait })).rejects.toThrow(error);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("does not infer safe fallback from a submit 404", async () => {
    const calls = scripted([]);
    calls.submit.mockRejectedValueOnce(new Error("HTTP 404 Not Found"));
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait })).rejects.toThrow("HTTP 404 Not Found");
    expect(calls.sync).not.toHaveBeenCalled();
    expect(calls.get).not.toHaveBeenCalled();
  });

  it("returns the answer of a server that ignored defer", async () => {
    const calls = scripted([]);
    calls.submit.mockResolvedValueOnce('{"selected_model":"m","response":{}}');
    expect(await runWithJobProgress(calls, () => undefined, { wait: noWait })).toBe('{"selected_model":"m","response":{}}');
    expect(calls.get).not.toHaveBeenCalled();
  });

  it("cancels the job when the client cancels the request", async () => {
    const calls = scripted([{ status: "running" }]);
    const controller = new AbortController();
    const wait = async () => { controller.abort(); };
    await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait })).rejects.toThrow(/cancelled by the client/);
    expect(calls.cancel).toHaveBeenCalledWith(ID);
  });

  it("does not submit an already-cancelled request", async () => {
    const calls = scripted([]);
    const controller = new AbortController();
    controller.abort();
    await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal })).rejects.toThrow(/before submission/);
    expect(calls.submit).not.toHaveBeenCalled();
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("does not fall back to fresh synchronous work after cancellation during submission", async () => {
    const calls = scripted([]);
    const controller = new AbortController();
    calls.submit.mockImplementation(async () => {
      controller.abort();
      throw new Error('{"code":"task_jobs_full"}');
    });
    await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal })).rejects.toThrow(/cancelled by the client/);
    expect(calls.sync).not.toHaveBeenCalled();
  });

  it("requests cancellation immediately while a poll is pending without claiming physical stop", async () => {
    const calls = scripted([]);
    const controller = new AbortController();
    let finishPoll!: (raw: string) => void;
    let startedPoll!: () => void;
    const polling = new Promise<void>((resolve) => { startedPoll = resolve; });
    calls.get.mockImplementation(() => {
      startedPoll();
      return new Promise<string>((resolve) => { finishPoll = resolve; });
    });
    const pending = runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait: noWait });
    await polling;
    controller.abort();
    expect(calls.cancel).toHaveBeenCalledOnce();
    finishPoll(receipt({ status: "running" }));
    await expect(pending).rejects.toThrow(/cancellation was requested.*Upstream stop is not confirmed/);
    expect(getEventListeners(controller.signal, "abort")).toHaveLength(0);
  });

  describe("cancellation after a client abort", () => {
    /** Aborts on the poll wait (which receives the signal) and records every wait, backoff included. */
    function abortingWait(controller: AbortController) {
      const waits: number[] = [];
      return { waits, wait: async (ms: number, signal?: AbortSignal) => { waits.push(ms); if (signal) controller.abort(); } };
    }

    it("retries a failed cancel request with backoff and reports it acknowledged", async () => {
      const calls = scripted([]);
      calls.cancel.mockRejectedValueOnce(new Error("connection lost")).mockRejectedValueOnce(new Error("connection lost"));
      const controller = new AbortController();
      const { waits, wait } = abortingWait(controller);
      const onJob = vi.fn();
      await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait, onJob }))
        .rejects.toThrow(/cancellation was requested for deferred job .* and acknowledged by serve/);
      expect(calls.cancel).toHaveBeenCalledTimes(3);
      expect(waits).toEqual([250, 250, 500]);
      expect(onJob.mock.calls).toEqual([[ID, true], [ID, false]]);
    });

    it("gives up after bounded attempts, reports the cancellation unknown and keeps the job open for outer cleanup", async () => {
      const calls = scripted([]);
      calls.cancel.mockRejectedValue(new Error("connection lost"));
      const controller = new AbortController();
      const { wait } = abortingWait(controller);
      const onJob = vi.fn();
      const failure = await runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait, onJob }).catch((error: Error) => error);
      expect((failure as Error).message).toMatch(/cancellation acknowledgement is unknown for deferred job .*may still be running/);
      expect((failure as Error).message).not.toContain("no cancel request reached");
      expect((failure as Error).message).not.toMatch(/cancellation was requested/);
      expect(calls.cancel).toHaveBeenCalledTimes(3);
      expect(onJob.mock.calls).toEqual([[ID, true]]);
    });

    it("reports a cancel serve accepted but has not finished as requested, not acknowledged", async () => {
      const calls = scripted([]);
      calls.cancel.mockResolvedValue(receipt({ status: "running" }));
      const controller = new AbortController();
      const onJob = vi.fn();
      const failure = await runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait: abortingWait(controller).wait, onJob })
        .catch((error: Error) => error);
      expect((failure as Error).message).toMatch(/cancellation was requested for deferred job/);
      expect((failure as Error).message).not.toMatch(/acknowledged/);
      expect(calls.cancel).toHaveBeenCalledOnce();
      expect(onJob.mock.calls).toEqual([[ID, true]]);
    });

    it("does not retry a cancel for a job serve no longer holds", async () => {
      const calls = scripted([]);
      calls.cancel.mockRejectedValue(new Error("HTTP 404 Not Found"));
      const controller = new AbortController();
      const onJob = vi.fn();
      await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait: abortingWait(controller).wait, onJob }))
        .rejects.toThrow(/cancellation acknowledgement is unknown/);
      expect(calls.cancel).toHaveBeenCalledOnce();
      expect(onJob.mock.calls).toEqual([[ID, true], [ID, false]]);
    });

    it("hands the cancellation state to the caller's error", async () => {
      const calls = scripted([]);
      calls.cancel.mockRejectedValue(new Error("connection lost"));
      const controller = new AbortController();
      const aborted = vi.fn((_id?: string, _state?: string) => new Error("stopped"));
      await expect(runWithJobProgress(calls, () => undefined, { signal: controller.signal, wait: abortingWait(controller).wait, aborted }))
        .rejects.toThrow("stopped");
      expect(aborted).toHaveBeenCalledWith(ID, "unknown");
    });
  });

  it("marks a submit with no answer from serve as unknown outcome, keeps a typed refusal whole, never replays", async () => {
    const calls = scripted([]);
    calls.submit.mockRejectedValueOnce(new Error("socket hang up"));
    const failure = await runWithJobProgress(calls, () => undefined, { wait: noWait }).catch((error: Error) => error);
    expect(JSON.parse((failure as Error).message)).toEqual({ error: "socket hang up", execution_outcome: "unknown", retry_safe: false });
    // Serve answered with a typed refusal: the deferred submit created no job, so nothing ran.
    const refusal = new Error(JSON.stringify({ code: "invalid_request", error: "bad input" }));
    calls.submit.mockRejectedValueOnce(refusal);
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait })).rejects.toBe(refusal);
    expect(calls.sync).not.toHaveBeenCalled();
    expect(calls.get).not.toHaveBeenCalled();
  });

  it("keeps a submit failure's own execution outcome", async () => {
    const calls = scripted([]);
    const own = new Error(JSON.stringify({ code: "model_unavailable", error: "no such model", execution_outcome: "not_started", retry_safe: true }));
    calls.submit.mockRejectedValueOnce(own);
    await expect(runWithJobProgress(calls, () => undefined, { wait: noWait })).rejects.toBe(own);
  });

  it("removes completed poll wait listeners", async () => {
    vi.useFakeTimers();
    try {
      const calls = scripted([{ status: "running" }, { status: "completed", result: { ok: true } }]);
      const controller = new AbortController();
      const pending = runWithJobProgress(calls, () => undefined, { signal: controller.signal });
      await vi.runAllTimersAsync();
      expect(JSON.parse(await pending)).toEqual({ ok: true });
      expect(getEventListeners(controller.signal, "abort")).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("tolerates a few failed polls and reports a persistent failure", async () => {
    const flaky = scripted([{ status: "completed", result: { ok: 1 } }]);
    flaky.get.mockRejectedValueOnce(new Error("connection reset")).mockRejectedValueOnce(new Error("connection reset"));
    expect(JSON.parse(await runWithJobProgress(flaky, () => undefined, { wait: noWait }))).toEqual({ ok: 1 });
    const down = scripted([]);
    down.get.mockRejectedValue(new Error("connection refused"));
    await expect(runWithJobProgress(down, () => undefined, { wait: noWait })).rejects.toThrow("connection refused");
    // Nobody follows the job any more, so it is cancelled rather than left running.
    expect(down.cancel).toHaveBeenCalledWith(ID);
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
