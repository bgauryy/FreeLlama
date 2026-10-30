import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { libraryTrialCandidate, parseModelCard, parseModelSearch, parseModelTags } from "../../src/model-search.js";

const tagsHtml = `<a class="md:hidden flex" href="/library/gemma3:1b">
 <span>gemma3:1b</span><span class="font-mono">abcdef123456</span> • 815MB • 32K context window •
 <span class="hidden sm:inline">Text input • 1 year ago</span>
 <div class="flex sm:hidden">Text input • 1 year ago</div></a>
 <a href="/library/gemma3:1b">gemma3:1b</a>
 <a class="md:hidden flex" href="/library/gemma3:4b-cloud">
 <span>gemma3:4b-cloud</span> • cloud • 128K context window • Text, Image input • today</a>`;

describe("Ollama library parsing", () => {
  it("does not equate cloud availability with cloud-only and supports public namespaces", () => {
    const result = parseModelSearch('<li class="result"><a href="/someone/model"><p class="max-w-lg">Coding model</p><span class="text-cyan-500">cloud</span><span class="text-indigo-600">tools</span></a></li>');
    expect(result[0]).toMatchObject({ name: "someone/model", cloudAvailable: true, description: "Coding model" });
    expect(result[0]).not.toHaveProperty("cloudOnly");
  });
  it("parses the October 2026 search cards by meaning: description, capability and size chips, counts", () => {
    const html = readFileSync(new URL("../fixtures/ollama-search-2026-10.html", import.meta.url), "utf8");
    const [qwen, embedding] = parseModelSearch(html);
    expect(qwen).toMatchObject({ name: "qwen3.8", capabilities: ["vision", "tools", "thinking"], sizes: ["27b"],
      pulls: "5.3M", downloads: 5_329_359, tags: "13", cloudAvailable: false });
    expect(qwen.description).toMatch(/^Qwen3\.8 delivers substantial gains/);
    expect(embedding).toMatchObject({ name: "qwen3-embedding", capabilities: ["embedding"], sizes: ["0.6b", "4b", "8b"],
      downloads: 4_444_715, tags: "12" });
  });

  it("reads capabilities from their own chip row and reports other badges as labels", () => {
    const [card] = parseModelSearch('<li><a href="/library/beam"><div><h2><span>beam</span></h2></div>' +
      '<div><span class="rounded-md text-indigo-600">tools</span><span class="rounded-md text-cyan-500">cloud</span></div>' +
      '<p class="any">A model</p><div><span title="9 downloads"><span >9</span></span>' +
      '<span class="rounded-md text-blue-600">1m</span><span class="rounded-md text-emerald-600">free</span></div></a></li>');
    expect(card).toMatchObject({ capabilities: ["tools", "cloud"], labels: ["free"], sizes: ["1m"], cloudAvailable: true });
  });

  it("keeps a capability chip it has never seen and tells sizes from capabilities", () => {
    const [card] = parseModelSearch('<li><a href="/library/m"><p class="any">Desc</p>' +
      '<span class="rounded-md text-indigo-600">audio</span><span class="rounded-md text-blue-600">e4b</span>' +
      '<span class="rounded-md text-blue-600">8x7b</span></a></li>');
    expect(card).toMatchObject({ capabilities: ["audio"], sizes: ["e4b", "8x7b"] });
  });

  it("reads the complete tag page, deduplicates desktop links, and preserves variant identity", () => {
    const { tags } = parseModelTags(tagsHtml, "gemma3");
    expect(tags).toHaveLength(2);
    expect(tags[0]).toMatchObject({ tag: "gemma3:1b", digest: "abcdef123456", sizeBytes: 815e6,
      context: "32K", modalities: "Text", cloud: false, updated: "1 year ago" });
    expect(tags[1]).toMatchObject({ tag: "gemma3:4b-cloud", cloud: true, sizeBytes: null });
  });

  it("extracts decoded README text without executing markup or returning editor duplicates", () => {
    const card = parseModelCard(`<meta content="Coding &amp; reasoning" name="description">
      <span class="text-indigo-600">tools</span><div id="display"><p>Duplicate</p></div>
      <textarea id="editor">## Usage\nSupports coding &#38; summarization.\nRequires Ollama 0.6 or later.</textarea>`);
    expect(card.description).toBe("Coding & reasoning");
    expect(card.readme).toContain("coding & summarization");
    expect(card.readme).not.toContain("Duplicate");
    expect(card.capabilities).toEqual(["tools"]);
  });

  it("falls back to rendered README text and reports changed markup explicitly", () => {
    expect(parseModelCard('<div id="display"><p>Summarize text.</p><div><p>Nested paragraph.</p></div><script>bad()</script></div>').readme)
      .toBe("Summarize text.\nNested paragraph.");
    expect(() => parseModelCard("<html>Unrecognized markup</html>")).toThrow(/markup/);
  });
});

describe("library trial candidates", () => {
  it.each([true, false])("does not transfer another model's quality evidence (embedding=%s)", (embedding) => {
    const candidate = libraryTrialCandidate("embeddinggemma:latest", embedding);
    expect(candidate.tag).toBe("embeddinggemma:latest");
    expect(candidate.taskQuality).toBe("unmeasured");
    expect(candidate.evidence).toBe("download_size_preflight_only");
    expect(JSON.stringify(candidate)).not.toMatch(/nomic|qwen|recall|4096|bigger is NOT/);
    expect(candidate.caution).toContain("exact tag");
  });
});
