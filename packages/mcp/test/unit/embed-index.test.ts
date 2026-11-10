import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({ run: vi.fn(), inputs: [] as unknown[][] }));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn(),
}));
vi.mock("../../src/model-library.js", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  installedInventory: async () => [{ name: "embed:1", digest: "d1" }],
}));

import { EmbeddingIndex, cosine, rankByQuery } from "../../src/embed-index.js";
import { callTool } from "../../src/agent-cli/registry.js";

/** A deterministic fake embedding: which of two topics the text is about. */
const vectorOf = (text: string) => (/retry|backoff/.test(text) ? [1, 0.1] : /parse|flag/.test(text) ? [0.1, 1] : [0.5, 0.5]);

beforeEach(() => {
  fixture.inputs.length = 0;
  fixture.run.mockReset();
  fixture.run.mockImplementation(async (_target: string, request: { input: string[] }) => {
    fixture.inputs.push(request.input);
    // As the core does with return_embeddings: the query's vector first, then the items'.
    const query = (request as unknown as { nearest: { query: string } }).nearest.query;
    return JSON.stringify({ route: { selected_model: "embed:1" }, response: { embeddings: [vectorOf(query), ...request.input.map(vectorOf)] }, metrics: {} });
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

  it("ranks by cosine like the core", () => {
    expect(cosine([1, 0], [1, 0])).toBeCloseTo(1);
    const ranked = rankByQuery([1, 0], [{ label: "far", vector: [0, 1] }, { label: "near", vector: [0.9, 0.1] }], 1);
    expect(ranked).toEqual([{ index: 1, label: "near", score: expect.any(Number) }]);
  });

  it("embeds only new chunks on a repeated search over the same files", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "code-"));
    writeFileSync(path.join(dir, "retry.ts"), "export function retry() { /* exponential backoff */ }\n");
    writeFileSync(path.join(dir, "args.ts"), "export function parseFlags() { /* parse argv flags */ }\n");
    const search = { task: "embedding", model: "embed:1", files: [path.join(dir, "*.ts")], nearest: { query: "where is retry backoff", top: 1 } };
    const first = await callTool("run_task", search);
    expect(first.isError).toBeFalsy();
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
});
