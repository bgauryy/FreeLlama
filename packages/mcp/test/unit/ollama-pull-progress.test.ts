import { afterEach, describe, expect, it, vi } from "vitest";
import { pullProgressReporter } from "../../src/tools/ollama-lifecycle.js";

afterEach(() => vi.restoreAllMocks());

describe("Ollama pull notifications", () => {
  it("keeps progress increasing through byte resets, status changes and immediate success", () => {
    let now = 0;
    vi.spyOn(Date, "now").mockImplementation(() => now);
    const sendNotification = vi.fn(async (_notification: unknown) => undefined);
    const report = pullProgressReporter({ signal: new AbortController().signal, _meta: { progressToken: "pull" }, sendNotification })!;
    report({ status: "pulling manifest" });
    now += 500; report({ status: "pulling layer-a", digest: "a", completed: 9000, total: 10000 });
    now += 100; report({ status: "pulling layer-a", digest: "a", completed: 9500, total: 10000 }); // throttled
    now += 400; report({ status: "pulling layer-b", digest: "b", completed: 0, total: 100 });
    now += 500; report({ status: "pulling layer-b", digest: "b", completed: 100, total: 100 });
    now += 500; report({ status: "verifying sha256 digest" });
    report({ status: "success" }); // terminal status must not be throttled
    const params = sendNotification.mock.calls.map(([notification]) => (notification as any).params);
    expect(params.map((item) => item.progress)).toEqual([1, 2, 3, 4, 5, 6]);
    expect(params.every((item) => item.progressToken === "pull" && !("total" in item))).toBe(true);
    expect(params.map((item) => item.message)).toEqual([
      "pulling manifest", "pulling layer-a: 9000/10000 bytes (90%)", "pulling layer-b: 0/100 bytes (0%)",
      "pulling layer-b: 100/100 bytes (100%)", "verifying sha256 digest", "success",
    ]);
  });

  it("has no reporter without a progress token", () => {
    expect(pullProgressReporter({ signal: new AbortController().signal, sendNotification: vi.fn(async () => undefined) })).toBeUndefined();
  });
});
