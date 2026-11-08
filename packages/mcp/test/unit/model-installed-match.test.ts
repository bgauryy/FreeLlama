import { afterEach, describe, expect, it, vi } from "vitest";

// The compiled addon and serve client are not loadable under vitest; these paths do not use them.
vi.mock("../../src/native.js", () => ({}));
vi.mock("../../src/serve-client.js", () => ({}));
import { catalogText, catalogView, installedMatch, MODEL_CATALOG } from "../../src/model-catalog.js";
import { annotateTags, libraryTagSize } from "../../src/tools/models.js";

const digest = "abcdef123456" + "0".repeat(52);

afterEach(() => vi.unstubAllGlobals());

describe("installed match", () => {
  it("matches the same tag however it is spelled", () => {
    expect(installedMatch("glm-ocr:latest", [{ name: "glm-ocr" }])).toEqual({ installed: true });
    expect(installedMatch("glm-ocr", [{ name: "library/glm-ocr:latest" }])).toEqual({ installed: true });
    expect(installedMatch("hf.co/Org/Repo:Q4_K_M", [{ name: "hf.co/org/repo:q4_k_m" }])).toEqual({ installed: true });
    expect(installedMatch("gemma3:1b", [{ name: "gemma3:4b" }])).toEqual({ installed: false });
  });

  it("matches weights installed under another tag by digest, naming that tag", () => {
    const inventory = [{ name: "my-gemma:latest", digest: `sha256:${digest}` }];
    expect(installedMatch("gemma3:1b", inventory, "abcdef123456")).toEqual({ installed: true, installed_as: "my-gemma:latest" });
    expect(installedMatch("gemma3:1b", inventory, "ffffff123456")).toEqual({ installed: false });
    // Too short to identify weights: never a match.
    expect(installedMatch("gemma3:1b", inventory, "abc")).toEqual({ installed: false });
    expect(installedMatch("gemma3:1b", [{ name: "my-gemma:latest" }], "abcdef123456")).toEqual({ installed: false });
  });

  it("annotates catalog picks by digest when a pick names one, and keeps the plain set form", () => {
    const pick = MODEL_CATALOG.jobs[0]!.picks[0]!;
    const original = (pick as { digest?: string }).digest;
    (pick as { digest?: string }).digest = digest;
    try {
      const view = catalogView(MODEL_CATALOG.jobs[0]!.job, [{ name: "renamed:latest", digest }], null);
      expect(view.jobs[0]!.picks[0]).toMatchObject({ tag: pick.tag, installed: true, installed_as: "renamed:latest" });
      expect(catalogText(view)).toContain("installed as renamed:latest");
    } finally {
      (pick as { digest?: string }).digest = original;
    }
    expect(catalogView(MODEL_CATALOG.jobs[0]!.job, new Set([pick.tag]), null).jobs[0]!.picks[0]).toMatchObject({ installed: true });
    expect(catalogView(MODEL_CATALOG.jobs[0]!.job, null, null).jobs[0]!.picks[0]).toMatchObject({ installed: null });
  });

  it("marks a library tag installed when its listed digest is installed under another name", () => {
    const tags = [{ tag: "gemma3:1b", sizeBytes: 815e6, cloud: false, digest: "abcdef123456" }, { tag: "gemma3:4b", sizeBytes: 3.3e9, cloud: false, digest: "999999999999" }];
    const { annotated } = annotateTags(tags, [{ name: "gemma3-small:latest", digest }], null, false);
    expect(annotated[0]).toMatchObject({ installed: true, installed_as: "gemma3-small:latest" });
    expect(annotated[1]).toMatchObject({ installed: false });
    expect(annotateTags(tags, null, null, false).annotated[0]).toMatchObject({ installed: null });
  });
});

describe("library tag size", () => {
  const card = `<meta name="description" content="Small models."><span class="text-indigo-600">tools</span><textarea id="editor">## Overview</textarea>`;
  const tagRows = `<a href="/library/gemma3x:1b" class="md:hidden"><span>gemma3x:1b</span>
    <span class="font-mono">abcdef123456</span> • 815MB • 32K context window • Text input • today</a>`;

  it("reads the size library step 2 lists, and tells not-found from unreachable", async () => {
    vi.stubGlobal("fetch", vi.fn(async (url: string | URL) => new Response(String(url).endsWith("/tags") ? tagRows : card,
      { headers: { "content-type": "text/html" } })));
    expect(await libraryTagSize("gemma3x:1b")).toMatchObject({ found: true, tag: "gemma3x:1b", sizeBytes: 815e6 });
    expect(await libraryTagSize("gemma3x:9b")).toMatchObject({ found: false, message: expect.stringMatching(/lists no tag gemma3x:9b/) });
    vi.stubGlobal("fetch", vi.fn(async () => new Response("", { status: 404 })));
    expect(await libraryTagSize("nosuchfamily:1b")).toMatchObject({ found: false });
    vi.stubGlobal("fetch", vi.fn(async () => { throw new Error("getaddrinfo ENOTFOUND ollama.com"); }));
    await expect(libraryTagSize("offlinefamily:1b")).rejects.toThrow(/ENOTFOUND/);
  });
});
