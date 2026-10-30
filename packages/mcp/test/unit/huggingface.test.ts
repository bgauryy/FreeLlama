import { describe, expect, it } from "vitest";
import { HuggingFaceClient, huggingFaceReference, parseHuggingFaceFiles, parseHuggingFaceSearch } from "../../src/huggingface.js";

describe("Hugging Face GGUF connector", () => {
  it("resolves only the hf.co/<owner>/<repo>[:quant] names Ollama pulls", () => {
    expect(huggingFaceReference("hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF:Q4_K_M")).toMatchObject({
      repo: "bartowski/Llama-3.2-1B-Instruct-GGUF", family: "hf.co/bartowski/Llama-3.2-1B-Instruct-GGUF", quant: "Q4_K_M" });
    expect(huggingFaceReference("huggingface.co/owner/repo")).toMatchObject({ repo: "owner/repo", quant: null });
    expect(huggingFaceReference("qwen3:8b")).toBeNull();
    expect(huggingFaceReference("hf.co/../repo")).toBeNull();
    expect(huggingFaceReference("hf.co/owner")).toBeNull();
  });

  it("groups GGUF files by the publisher's quant name, sums split shards and sets projectors apart", () => {
    const { tags, projectors } = parseHuggingFaceFiles([
      { type: "file", path: "README.md", size: 10 },
      { type: "file", path: "Llama-3.2-1B-Instruct-Q4_K_M.gguf", size: 807_694_464, lfs: { size: 807_694_464 } },
      { type: "file", path: "llama-2-7b.Q8_0.gguf", lfs: { size: 7_160_000_000 } },
      { type: "file", path: "Big-UD-Q4_K_XL-00001-of-00002.gguf", lfs: { size: 4e9 } },
      { type: "file", path: "Big-UD-Q4_K_XL-00002-of-00002.gguf", lfs: { size: 1e9 } },
      { type: "file", path: "BF16/model-00001-of-00002.gguf", lfs: { size: 2e9 } },
      { type: "file", path: "mmproj-model-f16.gguf", lfs: { size: 9e8 } },
      { type: "file", path: "notes.gguf", lfs: { size: 1 } },
      { type: "directory", path: "BF16" },
    ], "o/r");
    expect(tags.map((t) => [t.tag, t.sizeBytes, t.files])).toEqual([
      ["hf.co/o/r:notes.gguf", 1, 1],
      ["hf.co/o/r:Q4_K_M", 807_694_464, 1],
      ["hf.co/o/r:BF16", 2e9, 1],
      ["hf.co/o/r:UD-Q4_K_XL", 5e9, 2],
      ["hf.co/o/r:Q8_0", 7_160_000_000, 1],
    ]);
    expect(tags[0]).toMatchObject({ quant: null });
    expect(tags[1]).toMatchObject({ quant: "Q4_K_M", size: "808MB", cloud: false });
    expect(projectors).toEqual(["mmproj-model-f16.gguf"]);
  });

  it("never sums different files that share a quant, and treats quant names case-insensitively", () => {
    const { tags } = parseHuggingFaceFiles([
      { type: "file", path: "gemma-3-1b-q8_0.gguf", lfs: { size: 1e9 } },
      { type: "file", path: "qwen3-0.6b-q8_0.gguf", lfs: { size: 6e8 } },
      { type: "file", path: "tiny-Q4_0.gguf", lfs: { size: 3e8 } },
      { type: "file", path: "tiny-v2-q4_0.gguf", lfs: { size: 4e8 } },
      { type: "file", path: "solo-Q5_K_M.gguf", lfs: { size: 5e8 } },
    ], "ggml-org/models");
    expect(tags.map((t) => [t.tag, t.sizeBytes])).toEqual([
      ["hf.co/ggml-org/models:tiny-Q4_0.gguf", 3e8],
      ["hf.co/ggml-org/models:tiny-v2-q4_0.gguf", 4e8],
      ["hf.co/ggml-org/models:Q5_K_M", 5e8],
      ["hf.co/ggml-org/models:qwen3-0.6b-q8_0.gguf", 6e8],
      ["hf.co/ggml-org/models:gemma-3-1b-q8_0.gguf", 1e9],
    ]);
  });

  it("lists search hits under the name Ollama pulls", () => {
    expect(parseHuggingFaceSearch([{ id: "unsloth/Qwen3-8B-GGUF", pipeline_tag: "text-generation", downloads: 5, likes: 2 }, {}]))
      .toEqual([{ name: "hf.co/unsloth/Qwen3-8B-GGUF", source: "huggingface", task: "text-generation", downloads: 5, likes: 2, updated: null }]);
  });

  it("searches GGUF repos with the task filter and reports a missing repo as an error", async () => {
    const urls: string[] = [];
    const fetch = (async (url: string) => {
      urls.push(url);
      if (url.includes("/api/models/missing/")) return new Response("{}", { status: 404 });
      return new Response(JSON.stringify([{ id: "o/r", pipeline_tag: "feature-extraction" }]), { headers: { "content-type": "application/json" } });
    }) as typeof globalThis.fetch;
    const client = new HuggingFaceClient({ fetch, now: () => 0 });
    const result = await client.search("embed", { limit: 3, task: "feature-extraction" });
    expect(result.models[0].name).toBe("hf.co/o/r");
    const url = new URL(urls[0]);
    expect(Object.fromEntries(url.searchParams)).toMatchObject({ search: "embed", filter: "gguf", sort: "downloads", limit: "3", pipeline_tag: "feature-extraction" });
    expect(url.searchParams.getAll("expand[]")).toEqual(["downloads", "likes", "pipeline_tag", "lastModified"]);
    await expect(client.lookup("hf.co/missing/repo")).rejects.toThrow(/no public model/);
  });
});
