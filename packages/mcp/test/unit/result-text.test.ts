import { describe, expect, it } from "vitest";
import { residentModelsText, routeText } from "../../src/result-text.js";

describe("residentModelsText", () => {
  it("shows Ollama's placement label once", () => {
    const text = residentModelsText({ models: [
      { name: "a:2b", size: 2 ** 30, resident_context: 4096, placement: { processor: "100% GPU", gpu_percent: 100 } },
      { name: "b:9b", size: 2 ** 30, placement: { processor: "48%/52% CPU/GPU", gpu_percent: 52 } },
      { name: "c:1b", size: 2 ** 30, placement: { gpu_percent: 0 } },
    ] });
    expect(text).toContain("a:2b  size 1.0 GiB  100% GPU  ctx 4096");
    expect(text).not.toContain("100% GPU 100% GPU");
    expect(text).toContain("48%/52% CPU/GPU");
    expect(text).toContain("c:1b  size 1.0 GiB  0% GPU");
  });
});

describe("routeText", () => {
  it("distinguishes observed residency from absent evidence", () => {
    expect(routeText({ resident: true })).toContain(" (resident)");
    expect(routeText({ resident: false })).toContain(" (not resident)");
    expect(routeText({})).toContain(" (residency unknown)");
    expect(routeText({})).not.toContain(" (not resident)");
  });
});
