import { describe, expect, it } from "vitest";
import { batchProgressMessage } from "../../src/batch-progress.js";

describe("batch progress message", () => {
  it("counts done, failed, running and queued items", () => {
    expect(batchProgressMessage({ total: 8, done: 3, succeeded: 2, failed: 1, running: ["a", "b"], queued: 3, finished: false }))
      .toBe("batch: 3/8 done (1 failed), running: a, b, 3 queued");
  });

  it("omits empty parts", () => {
    expect(batchProgressMessage({ total: 2, done: 2, succeeded: 2, failed: 0, running: [], queued: 0, finished: true }))
      .toBe("batch: 2/2 done");
  });
});
