import { describe, expect, it } from "vitest";
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
    expect(result[0]).toMatchObject({ name: "someone/model", cloudAvailable: true, cloudOnly: null });
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
