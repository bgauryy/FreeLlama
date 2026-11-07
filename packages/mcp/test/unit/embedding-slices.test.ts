import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({
  run: vi.fn(), get: vi.fn(), cancel: vi.fn(),
  handlers: new Map<string, (...args: any[]) => Promise<any>>(),
}));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: fixture.get, cancelTaskJob: fixture.cancel,
  route: vi.fn(),
}));

import { embedInSlices, registerRunTaskTool } from "../../src/tools/run-task.js";
import { validateEmbeddingVectors } from "../../src/nearest.js";
import { embeddingInputParam } from "../../src/helpers.js";

const live = () => new AbortController().signal;
const inputs = (n: number) => Array.from({ length: n }, (_, i) => `item ${i}`);
const identity = () => ({ status: "verified", model_digest: "sha256:one", backend: "primary", upstream: "http://ollama:11434",
  dimensions: 2, effective_options: { options: {}, truncate: false, dimensions: null, observed_context_tokens: 2048 } });
function receipt(count: number, index = 1): any {
  return {
    embedding_identity: identity(), execution: { backend: "primary", model_digest: "sha256:one" },
    route: { selected_model: "embed:exact" },
    response: { model: "embed:exact", embeddings: Array.from({ length: count }, () => [1, 0]), total_duration: index * 10, load_duration: index, prompt_eval_count: count, prompt_eval_duration: index * 2 },
    metrics: { total_duration_ns: index * 10, load_duration_ns: index, prompt_tokens: count, cached_prompt_tokens: 0,
      uncached_prompt_tokens: count, prompt_duration_ns: index * 2, output_tokens: 0, output_duration_ns: 0 },
    admission: { queue_wait_ms: index, resource_wait_ms: index * 2, transition_wait_ms: 0, total_wait_ms: index * 3 },
  };
}

beforeAll(() => {
  registerRunTaskTool({ registerTool: (name: string, _config: unknown, handler: (...args: any[]) => Promise<any>) => {
    fixture.handlers.set(name, handler);
  } } as unknown as McpServer);
});
beforeEach(() => {
  vi.restoreAllMocks();
  fixture.run.mockReset(); fixture.get.mockReset(); fixture.cancel.mockReset();
  fixture.cancel.mockResolvedValue("{}");
  fixture.run.mockImplementation(async (_target, request) => JSON.stringify(receipt(request.input?.length ?? 1, fixture.run.mock.calls.length)));
});

describe("sliced embedding space", () => {
  it("pins the selected tag/digest/backend/upstream, preserves controls, and aggregates every slice", async () => {
    const request = { task: "embedding", model: "caller:alias", session_id: "session", context_tokens: 2048, request_options: { options: { seed: 42 } } };
    const result = JSON.parse(await embedInSlices("http://serve", request, inputs(65), undefined, live()));
    expect(fixture.run).toHaveBeenCalledTimes(3);
    expect(fixture.run.mock.calls[0]![1]).toMatchObject({ model: "caller:alias", session_id: "session" });
    for (const [, next] of fixture.run.mock.calls.slice(1)) expect(next).toMatchObject({
      model: "embed:exact", expected_model_digest: "sha256:one", expected_execution_backend: "primary",
      expected_upstream: "http://ollama:11434", context_tokens: 2048, request_options: { options: { seed: 42 } },
    });
    expect(request.model).toBe("caller:alias");
    expect(result.response.embeddings).toHaveLength(65);
    expect(result.metrics).toMatchObject({ total_duration_ns: 60, load_duration_ns: 6, prompt_tokens: 65 });
    expect(result.response).toMatchObject({ total_duration: 60, prompt_eval_count: 65 });
    expect(result.admission).toMatchObject({ queue_wait_ms: 6, total_wait_ms: 18 });
    expect(result.embedding_requests).toBe(3);
    expect(result.embedding_slices).toHaveLength(3);
    expect(result.embedding_slices[0].response.embeddings).toBeUndefined();
    expect(result.embedding_slices[0].response.embeddings_omitted).toEqual({ count: 32, dimensions: 2 });
  });

  it("keeps missing phase measurements unknown", async () => {
    fixture.run.mockImplementation(async (_target, request) => {
      const value = receipt(request.input.length);
      if (fixture.run.mock.calls.length === 2) delete value.metrics.load_duration_ns;
      return JSON.stringify(value);
    });
    const result = JSON.parse(await embedInSlices("s", { task: "embedding" }, inputs(65), undefined, live()));
    expect(result.metrics.load_duration_ns).toBeNull();
  });

  it("compares option values independently of JSON object key order", async () => {
    fixture.run.mockImplementation(async (_target, request) => {
      const value = receipt(request.input.length);
      value.embedding_identity.effective_options.options = fixture.run.mock.calls.length === 1 ? { seed: 1, num_ctx: 2048 } : { num_ctx: 2048, seed: 1 };
      return JSON.stringify(value);
    });
    expect(JSON.parse(await embedInSlices("s", {}, inputs(65), undefined, live())).response.embeddings).toHaveLength(65);
  });

  it.each(["missing", "unverified", "context"])("refuses %s identity before submitting another slice", async (kind) => {
    fixture.run.mockImplementation(async (_target, request) => {
      const value = receipt(request.input.length);
      if (kind === "missing") delete value.embedding_identity;
      if (kind === "unverified") value.embedding_identity.status = "unverified";
      if (kind === "context") value.embedding_identity.effective_options.observed_context_tokens = null;
      return JSON.stringify(value);
    });
    await expect(embedInSlices("s", {}, inputs(65), undefined, live())).rejects.toThrow("identity is uncertain");
    expect(fixture.run).toHaveBeenCalledTimes(1);
  });

  it.each(["model", "digest", "backend", "upstream", "options", "dimension", "count", "value"])("rejects a changed %s without replay or a third slice", async (kind) => {
    fixture.run.mockImplementation(async (_target, request) => {
      const value = receipt(request.input.length);
      if (fixture.run.mock.calls.length === 2) {
        if (kind === "model") value.route.selected_model = "other:exact";
        if (kind === "digest") value.embedding_identity.model_digest = "sha256:two";
        if (kind === "backend") value.embedding_identity.backend = "cpu";
        if (kind === "upstream") value.embedding_identity.upstream = "http://other";
        if (kind === "options") value.embedding_identity.effective_options.options = { seed: 42 };
        if (kind === "dimension") value.response.embeddings[0] = [1, 2, 3];
        if (kind === "count") value.response.embeddings.pop();
        if (kind === "value") value.response.embeddings[0][0] = null;
      }
      return JSON.stringify(value);
    });
    try { await embedInSlices("s", {}, inputs(65), undefined, live()); expect.fail("mixed embeddings must fail"); }
    catch (error) {
      const failure = JSON.parse((error as Error).message);
      expect(failure.completed_inputs).toBe(32);
      expect(failure.embedding_slices).toHaveLength(2);
      expect(failure.retry_safe).toBe(false);
    }
    expect(fixture.run).toHaveBeenCalledTimes(2);
  });

  it("preserves a common deadline rather than resetting the budget per slice", async () => {
    let now = 0;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    fixture.run.mockImplementation(async (_target, request) => { now += 4_000; return JSON.stringify(receipt(request.input.length)); });
    await embedInSlices("s", { timeout_seconds: 10 }, inputs(65), undefined, live());
    expect(fixture.run.mock.calls.map((call) => call[1].timeout_seconds)).toEqual([10, 6, 2]);
  });

  describe("which failure is labelled a deadline", () => {
    const failureOf = async (run: Promise<string>) => {
      try { await run; } catch (error) { return JSON.parse((error as Error).message); }
      throw new Error("expected the embedding to fail");
    };

    it("labels the failure a deadline when the deadline stopped the work", async () => {
      const deadline = new AbortController();
      vi.spyOn(AbortSignal, "timeout").mockReturnValue(deadline.signal);
      fixture.run.mockImplementation(async (_target, request) => {
        deadline.abort();
        return JSON.stringify(receipt(request.input.length));
      });
      const failure = await failureOf(embedInSlices("s", { timeout_seconds: 10 }, inputs(65), undefined, live()));
      expect(failure.code).toBe("embedding_deadline_exceeded");
      expect(failure.completed_inputs).toBe(32);
    });

    it("keeps an identity failure that happened first even though the deadline fired too", async () => {
      const deadline = new AbortController();
      vi.spyOn(AbortSignal, "timeout").mockReturnValue(deadline.signal);
      fixture.run.mockImplementation(async (_target, request) => {
        const value = receipt(request.input.length);
        if (fixture.run.mock.calls.length === 2) { deadline.abort(); value.embedding_identity.model_digest = "sha256:two"; }
        return JSON.stringify(value);
      });
      const failure = await failureOf(embedInSlices("s", { timeout_seconds: 10 }, inputs(65), undefined, live()));
      expect(failure.code).toBe("embedding_slices_invalid");
      expect(failure.error).toContain("identity mismatch");
      expect(failure.cause).toBeUndefined();
    });

    it("keeps the client's cancellation when the client cancelled before any deadline", async () => {
      const client = new AbortController();
      fixture.run.mockImplementation(async (_target, request) => {
        client.abort();
        return JSON.stringify(receipt(request.input.length));
      });
      const failure = await failureOf(embedInSlices("s", { timeout_seconds: 10 }, inputs(65), undefined, client.signal));
      expect(failure.code).toBe("task_cancelled");
    });
  });
});

describe("cancellation without a progress subscription", () => {
  it("does not submit pre-cancelled input", async () => {
    const controller = new AbortController(); controller.abort();
    await expect(embedInSlices("s", {}, inputs(65), undefined, controller.signal)).rejects.toThrow("cancelled");
    expect(fixture.run).not.toHaveBeenCalled();
  });

  it("cancels an accepted embedding job by ID and does not submit later slices", async () => {
    const controller = new AbortController();
    fixture.run.mockImplementation(async () => {
      controller.abort();
      return JSON.stringify({ deferred: true, job: { id: "accepted-job", status: "queued" } });
    });
    try { await embedInSlices("s", {}, inputs(65), undefined, controller.signal); expect.fail("must cancel"); }
    catch (error) {
      const failure = JSON.parse((error as Error).message);
      expect(failure.accepted_job_ids).toEqual(["accepted-job"]);
      expect(failure.cause).toMatchObject({ job_id: "accepted-job", cancellation_requested: true, retry_safe: false });
    }
    expect(fixture.run).toHaveBeenCalledTimes(1);
    expect(fixture.run.mock.calls[0]![1]).toMatchObject({ defer: true, input: inputs(32) });
    expect(fixture.cancel).toHaveBeenCalledWith("s", "accepted-job");
  });

  it("uses a cancellable job for a normal call without progress, refusing capacity without replay", async () => {
    fixture.run.mockRejectedValue(new Error(JSON.stringify({ error: "full", code: "task_jobs_full" })));
    const result = await fixture.handlers.get("run_task")!({ task: "completion", prompt: "hello" }, { signal: new AbortController().signal });
    expect(result.isError).toBe(true);
    // Serve's own code and reason reach the caller, so it knows which limit refused the job.
    expect(result.structuredContent).toMatchObject({ code: "task_jobs_full", execution_outcome: "not_started", retry_safe: true });
    expect(String(result.structuredContent?.error)).toContain("Serve refused the job before execution: full");
    expect(fixture.run).toHaveBeenCalledTimes(1);
    expect(fixture.run.mock.calls[0]![1].defer).toBe(true);
  });

  it("does not claim cancellation during an unacknowledged submit is safe to retry", async () => {
    const controller = new AbortController();
    fixture.run.mockImplementation(async () => { controller.abort(); throw new Error("lost acknowledgement"); });
    const result = await fixture.handlers.get("run_task")!({ task: "completion", prompt: "hello" }, { signal: controller.signal });
    expect(result.structuredContent).toMatchObject({ code: "task_cancelled", execution_outcome: "unknown", retry_safe: false });
    expect(fixture.run).toHaveBeenCalledTimes(1);
  });

  it("does not replay a completed job whose result could not be retained", async () => {
    fixture.run.mockResolvedValue(JSON.stringify({ deferred: true, job: { id: "large-job", status: "failed",
      error: { error: "too large", code: "task_job_result_too_large" } } }));
    const result = await fixture.handlers.get("run_task")!({ task: "completion", prompt: "hello" }, { signal: new AbortController().signal });
    expect(result.isError).toBe(true);
    expect(result.structuredContent).toMatchObject({ code: "task_job_result_too_large", job_id: "large-job", retry_safe: false, result_recoverable: false });
    expect(fixture.run).toHaveBeenCalledTimes(1);
  });

  it("bounds concurrent jobs and preserves item order when they finish out of order", async () => {
    const finish = new Map<string, (value: string) => void>();
    fixture.run.mockImplementation((_target, request) => new Promise<string>((resolve) => finish.set(request.prompt, resolve)));
    const pending = fixture.handlers.get("run_task")!({ maxParallelism: 2,
      tasks: ["a", "b", "c"].map((id) => ({ id, independent: true, task: { prompt: id } })),
    }, { signal: new AbortController().signal });
    await vi.waitFor(() => expect(finish.size).toBe(2));
    finish.get("b")!(JSON.stringify({ response: { message: { content: "B" } } }));
    await vi.waitFor(() => expect(finish.has("c")).toBe(true));
    finish.get("c")!(JSON.stringify({ response: { message: { content: "C" } } }));
    finish.get("a")!(JSON.stringify({ response: { message: { content: "A" } } }));
    const result = await pending;
    expect(result.structuredContent.max_parallelism).toBe(2);
    expect(result.structuredContent.results.map((row: any) => [row.id, row.response.response.message.content])).toEqual([["a", "A"], ["b", "B"], ["c", "C"]]);
  });

  it("keeps Rust's 3:2:1 priority start order within the actual parallelism cap", async () => {
    const finish = new Map<string, () => void>();
    const started: string[] = [];
    let active = 0;
    let peak = 0;
    fixture.run.mockImplementation((_target, request) => new Promise<string>((resolve) => {
      started.push(request.prompt); active += 1; peak = Math.max(peak, active);
      finish.set(request.prompt, () => { active -= 1; resolve(JSON.stringify({ response: { message: { content: request.prompt } } })); });
    }));
    const tasks = ["B1", "N1", "N2", "I1", "I2", "I3", "I4"].map((id) => ({
      id, independent: true, task: { prompt: id, priority: id.startsWith("I") ? "interactive" : id.startsWith("N") ? "normal" : "background" },
    }));
    const pending = fixture.handlers.get("run_task")!({ maxParallelism: 2, tasks }, { signal: new AbortController().signal });
    await vi.waitFor(() => expect(started).toEqual(["I1", "I2"]));
    for (const [completed, next] of [["I1", "I3"], ["I2", "N1"], ["I3", "N2"], ["N1", "B1"], ["N2", "I4"]]) {
      finish.get(completed!)!();
      await vi.waitFor(() => expect(started.at(-1)).toBe(next));
    }
    finish.get("B1")!(); finish.get("I4")!();
    const result = await pending;
    expect(peak).toBe(2);
    expect(started).toEqual(["I1", "I2", "I3", "N1", "N2", "B1", "I4"]);
    expect(result.structuredContent.results.map((row: any) => row.id)).toEqual(tasks.map((task) => task.id));
    expect(result.structuredContent.scheduler.weights).toEqual({ interactive: 3, normal: 2, background: 1 });
  });

  it("keeps batch order/defaults and does not dispatch queued items after cancellation", async () => {
    const controller = new AbortController();
    fixture.run.mockImplementation(async () => {
      controller.abort();
      return JSON.stringify({ deferred: true, job: { id: "batch-job", status: "queued" } });
    });
    const result = await fixture.handlers.get("run_task")!({ task: "completion", model: "pinned:tag", maxParallelism: 1,
      tasks: ["a", "b", "c"].map((id) => ({ id, independent: true, task: { prompt: id } })),
    }, { signal: controller.signal });
    expect(result.structuredContent.results.map((row: any) => row.id)).toEqual(["a", "b", "c"]);
    expect(result.structuredContent.results[0]).toMatchObject({ ok: false, accepted_job_ids: ["batch-job"] });
    expect(result.structuredContent.results[1].error.execution_outcome).toBe("not_started");
    expect(fixture.run).toHaveBeenCalledTimes(1);
    expect(fixture.run.mock.calls[0]![1]).toMatchObject({ model: "pinned:tag", defer: true, prompt: "a" });
  });
});

describe("embedding vector validation", () => {
  it.each([[[1, 2]], [[1, 2], [1]], [[1, 2], []], [[1, 2], [NaN, 1]], [[1, 2], [Infinity, 1]], [[1, 2], [null, 1]]].map((vectors) => [vectors] as const))(
    "rejects malformed vectors %j", (vectors) => expect(() => validateEmbeddingVectors(vectors, 3)).toThrow(),
  );
  it("rejects sparse numeric vectors", () => expect(() => validateEmbeddingVectors([new Array(2)], 1)).toThrow("nonnumeric"));
  it("accepts explicit mixed modalities while rejecting unknown embedding fields", () => {
    expect(embeddingInputParam.parse([{ text: "caption", image: "bytes" }, { audio: "wav" }, "plain"])).toHaveLength(3);
    for (const input of [{}, { video: "bytes" }, { image: "" }]) expect(embeddingInputParam.safeParse(input).success).toBe(false);
  });
});
