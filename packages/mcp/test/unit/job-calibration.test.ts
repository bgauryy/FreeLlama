import { afterEach, describe, expect, it, vi } from "vitest";
import { calibratedJobPick } from "../../src/job-calibration.js";

vi.mock("../../src/serve-http.js", () => ({ serveAuthHeaders: async () => ({ authorization: "Bearer fixture" }) }));
afterEach(() => vi.unstubAllGlobals());

function reply(value: unknown, status = 200) {
  const fetch = vi.fn(async () => new Response(JSON.stringify(value), { status }));
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

describe("machine-calibrated job selection", () => {
  it("uses Rust's locally measured route, including a model absent from the shipped list", async () => {
    const fetch = reply({ selected_model: "my-local:3b", task_evidence: "strong", quality_evidence: "none" });
    const result = await calibratedJobPick("http://localhost:9000", "coding", "default", ["tools"]);
    expect(result?.model).toBe("my-local:3b");
    expect(result?.note).toContain("does not establish this job's accuracy");
    const [url, options] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("http://localhost:9000/_freellama/v1/routes");
    expect(JSON.parse(String(options.body))).toEqual({ task: "coding", objective: "fastest", required_capabilities: ["tools"] });
    expect(options.headers).toMatchObject({ authorization: "Bearer fixture" });
    expect(options.redirect).toBe("error");
  });

  it("requires task quality evidence for capable workflows and specialized OCR", async () => {
    reply({ selected_model: "model", task_evidence: "strong", quality_evidence: "none" });
    expect(await calibratedJobPick(undefined, "coding", "capable", [])).toBeUndefined();
    expect(await calibratedJobPick(undefined, "vision", "default", [], true)).toBeUndefined();
    reply({ selected_model: "model", task_evidence: "strong", quality_evidence: "strong" });
    expect((await calibratedJobPick(undefined, "coding", "capable", []))?.model).toBe("model");
  });

  it("falls back when local measurement is missing, refusal occurs, or serve is unavailable", async () => {
    for (const route of [{ selected_model: "model", task_evidence: "none" }, { task_evidence: "strong" }, null]) {
      reply(route);
      expect(await calibratedJobPick(undefined, "completion", "default", [])).toBeUndefined();
    }
    reply({ selected_model: "model", task_evidence: "strong" }, 503);
    expect(await calibratedJobPick(undefined, "completion", "default", [])).toBeUndefined();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    expect(await calibratedJobPick(undefined, "completion", "default", [])).toBeUndefined();
  });
});
