import { describe, expect, it, vi } from "vitest";
import { enrichInstalledModels, libraryReference, modelKnowledge, OllamaLibraryClient, publicModelGuidance,
  type LibraryRecord } from "../../src/model-knowledge.js";

const digest = "abcdef123456" + "0".repeat(52);
const cardHtml = `<meta name="description" content="Models for coding, reasoning, and summarization.">
  <span class="text-indigo-600">vision</span><span class="text-indigo-600">tools</span>
  <textarea id="editor">## Overview
Supports coding and question answering.
Supports image understanding and tool calling.
Requires Ollama 0.6 or later.
Only the 4B variant supports images.
Use a prefix for retrieval queries.
Does not support translation.
\x60\x60\x60
Example: supports translation.
\x60\x60\x60</textarea>`;
function tagHtml(family = "gemma3") {
  return `<a href="/library/${family}:1b" class="md:hidden"><span>${family}:1b</span>
    <span class="font-mono">abcdef123456</span> • 815MB • 32K context window • Text input • today</a>`;
}
function mockFetch(card = cardHtml, tags = tagHtml()) {
  return vi.fn<typeof fetch>(async (url) => new Response(String(url).endsWith("/tags") ? tags : card,
    { headers: { "content-type": "text/html; charset=utf-8" } }));
}
async function fixtureRecord() { return new OllamaLibraryClient({ fetch: mockFetch(), now: () => 0 }).lookup("gemma3:1b"); }

describe("public model identity", () => {
  it.each(["gemma3", "gemma3:latest", "library/gemma3"])("normalizes only the explicit latest alias: %s", (name) => {
    expect(libraryReference(name)).toEqual({ family: "gemma3", tag: "gemma3:latest", url: "https://ollama.com/library/gemma3" });
  });
  it("supports exact community namespaces without sending them to /library", () => {
    expect(libraryReference("someone/model:Q4_K_M")).toEqual({ family: "someone/model", tag: "someone/model:Q4_K_M", url: "https://ollama.com/someone/model" });
  });
  it.each(["../model", "model/../../bad", "https://example.com/model", "model%2Fname", "model?q=x", "/model", "a/b/c"])
    ("does not turn an unrecognized name into a network target: %s", async (name) => {
      const fetcher = mockFetch();
      expect((await new OllamaLibraryClient({ fetch: fetcher }).lookup(name)).status).toBe("unmatched");
      expect(fetcher).not.toHaveBeenCalled();
    });
});

describe("bounded Ollama library fetch and cache", () => {
  it("fetches both the README and complete tag page with GET only and records provenance", async () => {
    const fetcher = mockFetch();
    const record = await new OllamaLibraryClient({ fetch: fetcher, now: () => 0 }).lookup("gemma3:1b");
    expect(record).toMatchObject({ status: "available", sourceUrl: "https://ollama.com/library/gemma3",
      tagsSourceUrl: "https://ollama.com/library/gemma3/tags", fetchedAt: "1970-01-01T00:00:00.000Z",
      expiresAt: "1970-01-01T01:00:00.000Z", cached: false });
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(fetcher.mock.calls.every(([url, init]) => String(url).startsWith("https://ollama.com/") &&
      init?.method === "GET" && init.redirect === "error" && init.signal instanceof AbortSignal)).toBe(true);
  });

  it("shares in-flight family lookups, expires success after an hour, and does not reuse expired data", async () => {
    const fetcher = mockFetch();
    let now = 0;
    const client = new OllamaLibraryClient({ fetch: fetcher, now: () => now });
    await Promise.all([client.lookup("gemma3:1b"), client.lookup("gemma3:4b")]);
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect((await client.lookup("gemma3:1b")).cached).toBe(true);
    now = 3_600_000;
    fetcher.mockImplementation(async () => new Response("gone", { status: 404 }));
    expect((await client.lookup("gemma3:1b")).status).toBe("not_found");
    expect(fetcher).toHaveBeenCalledTimes(4);
  });

  it("evicts old entries when the cache reaches its bound", async () => {
    const fetcher = vi.fn<typeof fetch>(async (url) => {
      const family = String(url).split("/")[4];
      return new Response(String(url).endsWith("/tags") ? tagHtml(family) : cardHtml, { headers: { "content-type": "text/html" } });
    });
    const client = new OllamaLibraryClient({ fetch: fetcher, maxEntries: 1 });
    await client.lookup("gemma3:1b");
    await client.lookup("other:1b");
    await client.lookup("gemma3:1b");
    expect(fetcher).toHaveBeenCalledTimes(6);
  });

  it.each([404, 403, 429, 500])("reports HTTP %s, caches failures briefly, and retries after a minute", async (status) => {
    let now = 0;
    const fetcher = vi.fn<typeof fetch>(async () => new Response("refused", { status }));
    const client = new OllamaLibraryClient({ fetch: fetcher, now: () => now });
    expect((await client.lookup("gemma3:1b")).status).toBe(status === 404 ? "not_found" : "unavailable");
    expect((await client.lookup("gemma3:1b")).cached).toBe(true);
    expect(fetcher).toHaveBeenCalledTimes(2);
    now = 60_000;
    await client.lookup("gemma3:1b");
    expect(fetcher).toHaveBeenCalledTimes(4);
  });

  it("reports transport failure, timeout, wrong content type, and changed markup explicitly", async () => {
    const fetchers: Array<typeof fetch> = [
      async () => { throw new Error("offline"); },
      async () => new Response("{}", { headers: { "content-type": "application/json" } }),
      mockFetch("<html>changed</html>"),
      mockFetch(cardHtml, "<html>changed</html>"),
    ];
    for (const fetcher of fetchers) {
      expect(["unavailable", "parse_error"]).toContain((await new OllamaLibraryClient({ fetch: fetcher }).lookup("gemma3:1b")).status);
    }
    const abortingFetch: typeof fetch = async (_url, init) => new Promise((_resolve, reject) => {
      init!.signal!.addEventListener("abort", () => reject(new Error("timeout")), { once: true });
    });
    expect((await new OllamaLibraryClient({ fetch: abortingFetch, timeoutMs: 5 }).lookup("gemma3:1b")).status).toBe("unavailable");
  });

  it("caps received bytes even without Content-Length and labels README truncation", async () => {
    const oversized = mockFetch("x".repeat(2 * 1024 * 1024 + 1));
    expect((await new OllamaLibraryClient({ fetch: oversized }).lookup("gemma3:1b")).status).toBe("unavailable");
    const largeCard = `<meta name="description" content="Coding"><textarea id="editor">${"x".repeat(70_000)}</textarea>`;
    const record = await new OllamaLibraryClient({ fetch: mockFetch(largeCard) }).lookup("gemma3:1b");
    expect(record.readmeTruncated).toBe(true);
    expect(record.card!.readme).toHaveLength(65_536);
    expect(publicModelGuidance(record, undefined, true)).toMatchObject({ readme: { truncated: true } });
  });
});

describe("supported features and advertised uses", () => {
  it.each([
    ["coding", "completion", "Designed for software engineering workflows."],
    ["tool_calling", "tools", "Supports function calling."],
    ["image_understanding", "vision", "Extract text with optical character recognition."],
    ["reasoning", "completion", "Suited to mathematical tasks."],
    ["embeddings", "embedding", "A text encoder for retrieval."],
    ["summarization", "completion", "Summarise long documents."],
    ["question_answering", "completion", "Use for question-answering."],
    ["translation", "completion", "Translate multilingual documents."],
    ["long_context", "completion", "Supports repository-scale understanding."],
  ])("grounds the advertised %s use in publisher prose and its required feature", async (task, capability, text) => {
    const base = await fixtureRecord();
    const record = { ...base, card: { ...base.card!, description: text, readme: "" } };
    const guidance = publicModelGuidance(record, [capability]);
    expect(guidance.advertisedUseCases).toEqual([expect.objectContaining({ task, scope: "family", quality: "unmeasured",
      evidence: expect.objectContaining({ excerpt: text, sourceUrl: record.sourceUrl, fetchedAt: record.fetchedAt }) })]);
    expect(publicModelGuidance(record, []).advertisedUseCases).toEqual([]);
  });

  it("keeps the matching claim inside the evidence window of a long paragraph", async () => {
    const base = await fixtureRecord();
    const record = { ...base, card: { ...base.card!, description: "", readme: `${"Background ".repeat(100)}supports coding workflows.` } };
    expect(publicModelGuidance(record, ["completion"]).advertisedUseCases[0].evidence.excerpt).toContain("coding");
  });

  it("keeps the exact local feature set including unknown future capabilities", async () => {
    const knowledge = modelKnowledge("gemma3:1b", { digest, capabilities: ["completion", "future-feature", "toString", "__proto__"] }, await fixtureRecord());
    expect(knowledge.supportedFeatures).toEqual(["completion", "future-feature", "toString", "__proto__"]);
    expect(knowledge.supportedUseCases.map((use) => use.use)).toEqual(["text_generation"]);
    expect(knowledge.library.status).toBe("matched");
    expect(knowledge.quality).toBe("metadata_only");
    const serialized = JSON.stringify(knowledge);
    expect(serialized).not.toContain('"task":"image_understanding"');
    expect(serialized).not.toContain('"task":"tool_calling"');
    expect(serialized).toContain('"task":"coding"');
    expect(serialized).not.toContain('"task":"translation"');
    expect(serialized).toContain('"scope":"family"');
    expect(serialized).toContain('"quality":"unmeasured"');
    expect(serialized).not.toContain('"readme":');
  });

  it.each(Object.entries({ completion: "text_generation", tools: "tool_calling", vision: "image_understanding",
    audio: "audio_input", embedding: "embeddings", thinking: "thinking", insert: "fill_in_middle", image: "image_generation" }))
    ("explains the supported %s use without claiming accuracy", (capability, use) => {
      expect(modelKnowledge("test:latest", { capabilities: [capability] }).supportedUseCases)
        .toEqual([expect.objectContaining({ capability, use, reason: expect.any(String), basis: "supported_feature" })]);
    });

  it.each([
    { data: { digest: "1".repeat(64) }, status: "digest_mismatch" },
    { data: {}, status: "unverified" },
    { data: { digest: "abc" }, status: "unverified" },
  ])("withholds family guidance when identity is $status", async ({ data, status }) => {
    const knowledge = modelKnowledge("gemma3:1b", { ...data, capabilities: ["completion"] }, await fixtureRecord(), true);
    expect(knowledge.library.status).toBe(status);
    expect(knowledge.library).not.toHaveProperty("advertisedUseCases");
    expect(knowledge.library).not.toHaveProperty("readme");
  });

  it("does not invent alias provenance or confuse model architecture with a public family", async () => {
    expect(modelKnowledge("custom:1b", { digest, capabilities: ["completion"] }, await fixtureRecord()).library.status).toBe("tag_not_found");
    expect(modelKnowledge("gemma3:4b", { digest, capabilities: ["completion"] }, await fixtureRecord()).library.status).toBe("tag_not_found");
  });

  it("preserves local results when the website is absent or unavailable", () => {
    for (const status of ["not_found", "unavailable", "parse_error"] as const) {
      const record: LibraryRecord = { status, sourceUrl: "https://ollama.com/library/custom", tagsSourceUrl: null,
        fetchedAt: null, expiresAt: null, cached: false };
      expect(modelKnowledge("custom:latest", { capabilities: ["embedding"] }, record)).toMatchObject({
        supportedFeatures: ["embedding"], library: { status } });
    }
  });

  it("returns full bounded README text only on request, with sourced usage notes", async () => {
    const knowledge = modelKnowledge("gemma3:1b", { digest, capabilities: ["completion"] }, await fixtureRecord(), true);
    expect(knowledge.library).toMatchObject({ readme: { text: expect.stringContaining("Requires Ollama 0.6"), format: "markdown", truncated: false },
      usageNotes: expect.arrayContaining([expect.objectContaining({ text: "Requires Ollama 0.6 or later.", scope: "family" })]) });
  });

  it.each([{ remote_host: "https://ollama.com" }, { remote_model: "big" }])("flags cloud aliases from actual remote metadata", (remote) => {
    expect(modelKnowledge("alias:latest", { ...remote, capabilities: ["tools"] })).toMatchObject({ localModel: false, library: { status: "remote_model" } });
  });
});

describe("installed model enrichment", () => {
  it("does no website or extra local calls by default and preserves benchmark evidence", async () => {
    const client = new OllamaLibraryClient({ fetch: mockFetch() });
    const lookup = vi.spyOn(client, "lookup");
    const loadTags = vi.fn();
    const [model] = await enrichInstalledModels([{ name: "gemma3:1b", digest, capabilities: ["completion"], benchmark: { tps: 42 }, policy_rank: 2 }], client, false, loadTags);
    expect(model).toMatchObject({ benchmark: { tps: 42 }, policy_rank: 2, knowledge: { library: { status: "not_requested" } } });
    expect(lookup).not.toHaveBeenCalled();
    expect(loadTags).not.toHaveBeenCalled();
  });

  it("reads each assigned backend once, preserves future features, and skips cloud aliases", async () => {
    const client = new OllamaLibraryClient({ fetch: mockFetch() });
    const lookup = vi.spyOn(client, "lookup");
    const models = [
      { name: "gemma3:1b", digest, capabilities: ["completion"], execution: { upstream: "http://cpu" } },
      { name: "remote-alias:latest", digest, capabilities: ["tools"], execution: { upstream: "http://cpu" } },
    ];
    const loadTags = vi.fn(async () => [
      { name: "gemma3:1b", digest, capabilities: ["completion", "future"] },
      { name: "remote-alias:latest", digest, remote_host: "https://ollama.com" },
    ]);
    const result = await enrichInstalledModels(models, client, true, loadTags);
    expect(loadTags).toHaveBeenCalledExactlyOnceWith("http://cpu");
    expect(lookup).toHaveBeenCalledExactlyOnceWith("gemma3:1b");
    expect(result[0]).toMatchObject({ knowledge: { supportedFeatures: ["completion", "future"], library: { status: "matched" } } });
    expect(result[1]).toMatchObject({ knowledge: { localModel: false, library: { status: "remote_model" } } });
  });

  it("withholds public guidance when the backend inventory drifts or cannot be read", async () => {
    const client = new OllamaLibraryClient({ fetch: mockFetch() });
    const lookup = vi.spyOn(client, "lookup");
    for (const loadTags of [async () => [], async () => [{ name: "gemma3:1b", digest: "wrong" }], async () => { throw new Error("offline"); }]) {
      const result = await enrichInstalledModels([{ name: "gemma3:1b", digest, capabilities: ["completion"] }], client, true, loadTags);
      expect(result[0]).toMatchObject({ knowledge: { supportedFeatures: ["completion"], library: { status: "unverified" } } });
    }
    expect(lookup).not.toHaveBeenCalled();
  });

  it("bounds family enrichment concurrency", async () => {
    const client = new OllamaLibraryClient();
    let active = 0;
    let maximum = 0;
    vi.spyOn(client, "lookup").mockImplementation(async () => {
      maximum = Math.max(maximum, ++active);
      await new Promise((resolve) => setTimeout(resolve, 2));
      active--;
      return { status: "unavailable", sourceUrl: null, tagsSourceUrl: null, fetchedAt: null, expiresAt: null, cached: false };
    });
    await enrichInstalledModels(Array.from({ length: 9 }, (_, index) => ({ name: `model${index}:latest` })), client, true);
    expect(maximum).toBe(4);
    expect(active).toBe(0);
  });
});
