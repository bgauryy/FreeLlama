import { describe, expect, it } from "vitest";
import { taskNotices } from "../../src/progress.js";

describe("task notices", () => {
  it("stays quiet for a quick call on a loaded model", () => {
    expect(taskNotices({ route: { selected_model: "m" }, metrics: { load_duration_ns: 2e6 }, admission: { total_wait_ms: 10 } })).toEqual([]);
  });

  it("reports a slow load, what was unloaded for it, and a long wait", () => {
    expect(taskNotices({
      route: { selected_model: "glm-ocr:latest" },
      metrics: { load_duration_ns: 2.4e9 },
      admission: { total_wait_ms: 45_000 },
      execution: { memory_reservation: { evicted_idle_models: ["nomic-embed-text:v1.5"] } },
    })).toEqual([
      "loaded glm-ocr:latest in 2.4 s; unloaded nomic-embed-text:v1.5 to make room",
      "glm-ocr:latest waited 45.0 s for capacity before running",
    ]);
  });
});
