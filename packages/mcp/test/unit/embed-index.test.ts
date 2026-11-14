import { appendFileSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({ run: vi.fn(), inputs: [] as unknown[][],
  digest: "d1", backend: "primary", options: { num_ctx: 4096 }, verified: true, driftAfterProbe: false }));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn(),
}));


import { EmbeddingIndex, cosine, rankByQuery } from "../../src/embed-index.js";
import { callTool } from "../../src/agent-cli/registry.js";

/** A deterministic fake embedding: which of two topics the text is about. */
const vectorOf = (text: string) => (/retry|backoff/.test(text) ? [1, 0.1] : /parse|flag/.test(text) ? [0.1, 1] : [0.5, 0.5]);

beforeEach(() => {
  fixture.inputs.length = 0;
  fixture.digest = "d1"; fixture.backend = "primary"; fixture.options = { num_ctx: 4096 };
  fixture.verified = true; fixture.driftAfterProbe = false;
  fixture.run.mockReset();
  fixture.run.mockImplementation(async (target: string, request: { input: string[]; nearest?: { query: string }; expected_model_digest?: string }) => {
    fixture.inputs.push(request.input);
    const vectors = request.input.map(vectorOf);
    const embeddings = request.nearest ? [vectorOf(request.nearest.query), ...vectors] : vectors;
    return JSON.stringify({ route: { selected_model: "embed:1" }, response: { embeddings }, metrics: {},
      ...(fixture.verified ? { embedding_identity: { status: "verified", model_digest: fixture.driftAfterProbe && request.expected_model_digest ? "changed" : fixture.digest,
        backend: fixture.backend, upstream: target, dimensions: 2, effective_options: { options: fixture.options,
          truncate: false, dimensions: null, observed_context_tokens: fixture.options.num_ctx } } } : {}) });
  });
});

describe("embedding index", () => {
  it("round-trips vectors through its file and keeps identities apart", () => {
    const root = mkdtempSync(path.join(tmpdir(), "index-"));
    const index = EmbeddingIndex.open("m", "d", undefined, root);
    index.add([{ text: "a", vector: [0.5, -0.25] }]);
    expect(EmbeddingIndex.open("m", "d", undefined, root).get("a")).toEqual([0.5, -0.25]);
    expect(EmbeddingIndex.open("m", "other-digest", undefined, root).get("a")).toBeUndefined();
    expect(EmbeddingIndex.open("m", "d", 128, root).get("a")).toBeUndefined();
  });

  it("ignores corrupted, nonfinite or wrong-dimension cached vectors", () => {
    const root = mkdtempSync(path.join(tmpdir(), "index-corrupt-"));
    const index = EmbeddingIndex.open("m", "d", 2, root);
    index.add([{ text: "good", vector: [0.5, -0.25] }]);
    const encoded = (vector: number[]) => Buffer.from(new Float32Array(vector).buffer).toString("base64");
    appendFileSync(index.file, [
      JSON.stringify({ k: EmbeddingIndex.key("nan"), v: encoded([Number.NaN, 1]) }),
      JSON.stringify({ k: EmbeddingIndex.key("wrong"), v: encoded([1]) }),
      JSON.stringify({ k: EmbeddingIndex.key("broken"), v: "x" }),
      "{torn",
    ].join("\n") + "\n");
    const loaded = EmbeddingIndex.open("m", "d", 2, root);
    expect(loaded.get("good")).toEqual([0.5, -0.25]);
    for (const text of ["nan", "wrong", "broken"]) expect(loaded.get(text)).toBeUndefined();
  });

  it("ranks by cosine like the core", () => {
    expect(cosine([1, 0], [1, 0])).toBeCloseTo(1);
    const ranked = rankByQuery([1, 0], [{ label: "far", vector: [0, 1] }, { label: "near", vector: [0.9, 0.1] }], 1);
    expect(ranked).toEqual([{ index: 1, label: "near", score: expect.any(Number) }]);
  });

  it("separates actual endpoint, backend and effective-option identities", () => {
    const root = mkdtempSync(path.join(tmpdir(), "index-"));
    const index = EmbeddingIndex.open("m", "d", 2, root, "endpoint-a:primary:num_ctx=4096");
    index.add([{ text: "a", vector: [0.5, -0.25] }]);
    for (const scope of ["endpoint-b:primary:num_ctx=4096", "endpoint-a:cpu:num_ctx=4096", "endpoint-a:primary:num_ctx=1024"]) {
      expect(EmbeddingIndex.open("m", "d", 2, root, scope).get("a")).toBeUndefined();
    }
    expect(() => index.add([{ text: "bad", vector: [1, Number.NaN] }])).toThrow(/finite vectors/);
    expect(() => index.add([{ text: "bad", vector: [1] }])).toThrow(/verified dimensions/);
    expect(() => cosine([1], [1, 2])).toThrow(/matching dimensions/);
  });

  it("embeds only new chunks on a repeated search over the same files", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* exponential backoff */ }\n");
    writeFileSync(path.join(dir, "args.ts"), "export function parseFlags() { /* parse argv flags */ }\n");
    const search = { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")], nearest: { query: "where is retry backoff", top: 1 } };
    const first = await callTool("run_task", search);
    expect(first.isError, JSON.stringify(first)).toBeFalsy();
    expect(first.structuredContent?.embedding_index).toMatchObject({ cached: 0, embedded: 2 });
    expect((first.content?.[0]?.text ?? "")).toMatch(/retry\.ts/);
    const second = await callTool("run_task", search);
    // Only the query is embedded the second time: every chunk came from the index.
    expect(fixture.inputs.at(-1)).toEqual(["where is retry backoff"]);
    expect(second.structuredContent?.embedding_index).toMatchObject({ cached: 2, embedded: 0 });
    expect((second.content?.[0]?.text ?? "")).toMatch(/retry\.ts/);
    writeFileSync(path.join(dir, "args.ts"), "export function parseFlags() { /* parse argv flags, now with --each */ }\n");
    const third = await callTool("run_task", search);
    expect(third.structuredContent?.embedding_index).toMatchObject({ cached: 1, embedded: 1 });
  });
  it("re-embeds documents when actual digest, endpoint, backend or effective options change", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "identity-code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* exponential backoff */ }\n");
    const search = { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")], nearest: { query: "retry" }, endpoint: "http://localhost:11435" };
    const check = async (request = search) => {
      const result = await callTool("run_task", request);
      expect(result.isError, JSON.stringify(result)).toBeFalsy();
      return result.structuredContent?.embedding_index as { cached: number; embedded: number };
    };
    expect(await check()).toMatchObject({ cached: 0, embedded: 1 });
    expect(await check()).toMatchObject({ cached: 1, embedded: 0 });
    fixture.digest = "d2";
    expect(await check()).toMatchObject({ cached: 0, embedded: 1 });
    fixture.backend = "cpu";
    expect(await check()).toMatchObject({ cached: 0, embedded: 1 });
    fixture.options = { num_ctx: 1024 };
    expect(await check()).toMatchObject({ cached: 0, embedded: 1 });
    expect(await check({ ...search, endpoint: "http://localhost:22435" })).toMatchObject({ cached: 0, embedded: 1 });
  });

  it("preserves explicit routing controls through the normal managed path", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "controlled-code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* backoff */ }\n");
    await callTool("run_task", { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")],
      nearest: { query: "retry" }, contextTokens: 2048, minConfidence: "medium", minPlacementEvidence: "observed" });
    expect(fixture.run).toHaveBeenCalledTimes(1);
    expect(fixture.run.mock.calls[0]![1]).toMatchObject({ context_tokens: 2048, min_confidence: "medium", min_placement_evidence: "observed" });
    expect(fixture.inputs[0]![0]).toContain("export function retry");
  });

  it("answers without reuse when the actual embedding identity is unverified", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "unknown-code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* backoff */ }\n");
    fixture.verified = false;
    const search = { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")], nearest: { query: "retry" } };
    for (let i = 0; i < 2; i++) {
      const result = await callTool("run_task", search);
      expect(result.isError, JSON.stringify(result)).toBeFalsy();
      expect(result.structuredContent?.embedding_index).toMatchObject({ status: "disabled_unverified_identity", cached: 0, embedded: 1 });
    }
  });

  it("rejects identity drift before mixing query, cached and new document vectors", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "drift-code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* backoff */ }\n");
    fixture.driftAfterProbe = true;
    const result = await callTool("run_task", { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")], nearest: { query: "retry" } });
    expect(result.isError).toBe(true);
    expect(result.content?.[0]?.text).toMatch(/identity changed/);
    expect(result.structuredContent).toMatchObject({ retry_safe: false });
  });

});
