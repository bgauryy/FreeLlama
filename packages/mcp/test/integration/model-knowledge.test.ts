import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

describe("model knowledge through the built MCP server", () => {
  let client: Client;
  let server: Server;
  let endpoint: string;
  const requests: string[] = [];
  const digest = "abcdef123456" + "0".repeat(52);
  const local = { name: "gemma3:1b", digest, capabilities: ["completion", "future-feature"], size: 815e6 };
  const card = `<meta name="description" content="Coding and summarization models.">
    <span class="text-indigo-600">vision</span>
    <textarea id="editor">Supports coding and image understanding.\nRequires Ollama 0.6 or later.</textarea>`;
  const tags = `<a href="/library/gemma3:1b" class="md:hidden"><span>gemma3:1b</span>
    <span class="font-mono">abcdef123456</span> • 815MB • 32K context window • Text input • today</a>
    <a href="/library/gemma3:4b" class="md:hidden"><span>gemma3:4b</span>
    <span class="font-mono">111111111111</span> • 3.3GB • 128K context window • Text, Image input • today</a>
    <a href="/library/gemma3:4b-cloud" class="md:hidden">gemma3:4b-cloud • cloud • 128K context window • Text, Image input • today</a>`;

  beforeAll(async () => {
    server = createServer(async (request, response) => {
      requests.push(request.url!);
      response.setHeader("content-type", "application/json");
      if (request.url!.startsWith("/library/")) {
        response.setHeader("content-type", "text/html");
        if (!request.url!.startsWith("/library/gemma3")) { response.statusCode = 404; response.end("missing"); return; }
        response.end(request.url!.endsWith("/tags") ? tags : card);
      } else if (request.url === "/api/show") {
        let raw = "";
        for await (const chunk of request) raw += chunk;
        const { model } = JSON.parse(raw);
        response.end(JSON.stringify({ capabilities: ["completion", "future-feature"],
          ...(model === "remote-alias:latest" ? { remote_host: "https://ollama.com", remote_model: "large" } : {}),
          model_info: { "general.architecture": "gemma3", "gemma3.context_length": 32768 },
          license: "license", modelfile: "FROM gemma3:1b" }));
      } else if (request.url === "/api/tags") {
        response.end(JSON.stringify({ models: [local, { name: "remote-alias:latest", digest, remote_host: "https://ollama.com" }] }));
      } else if (request.url === "/_freellama/v1/models") {
        response.end(JSON.stringify({ models: [{ ...local, execution: { upstream: endpoint }, benchmark: { tokens_per_second: 42 }, policy_rank: { coding: 0 } }] }));
      } else if (request.url === "/_freellama/v1/machine") {
        response.end(JSON.stringify({ memory_bytes: 10e9 }));
      } else { response.end(JSON.stringify({ status: "ok" })); }
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing fixture-server address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient({ TEST_OLLAMA_LIBRARY_ORIGIN: endpoint,
      NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ""} --import=${new URL("../fixtures/ollama-library-fetch.mjs", import.meta.url).href}` });
  });
  beforeEach(() => { requests.length = 0; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server?.close((error) => error ? reject(error) : resolve()));
  });

  async function models(args: Record<string, unknown>) {
    const result = await client.callTool({ name: "models", arguments: { endpoint, ollamaEndpoint: endpoint, ...args } });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    return result.structuredContent as any;
  }

  it("keeps default detail local and preserves forward-compatible capabilities", async () => {
    const result = await models({ view: "detail", model: "gemma3:1b" });
    expect(result.knowledge.supportedFeatures).toEqual(["completion", "future-feature"]);
    expect(result.knowledge.library.status).toBe("not_requested");
    expect(result.max_context_length).toBe(32768);
    expect(result).not.toHaveProperty("license");
    expect(requests).toEqual(["/api/show"]);
  });

  it("enriches a digest-matched text variant without inheriting family vision support", async () => {
    const result = await models({ view: "detail", model: "gemma3:1b", includeLibrary: true, includeReadme: true });
    expect(result.knowledge.library.status).toBe("matched");
    expect(result.knowledge.library.tag.modalities).toBe("Text");
    expect(result.knowledge.library.advertisedUseCases.map((use: any) => use.task)).toContain("coding");
    expect(result.knowledge.library.advertisedUseCases.map((use: any) => use.task)).not.toContain("image_understanding");
    expect(result.knowledge.library.readme.text).toContain("Requires Ollama");
    expect(result.knowledge.quality).toBe("metadata_only");
    expect(requests).toEqual(expect.arrayContaining(["/api/show", "/api/tags", "/library/gemma3", "/library/gemma3/tags"]));
  });

  it("reuses the cached card, keeps README optional, and leaves verbose Ollama fields available", async () => {
    await models({ view: "detail", model: "gemma3:1b", includeLibrary: true });
    requests.length = 0;
    const result = await models({ view: "detail", model: "gemma3:1b", includeLibrary: true, includeVerbose: true });
    expect(result.knowledge.library.cached).toBe(true);
    expect(result.knowledge.library).not.toHaveProperty("readme");
    expect(result.license).toBe("license");
    expect(requests).toEqual(["/api/show", "/api/tags"]);
  });

  it("enriches installed inventory while preserving benchmark and policy evidence", async () => {
    const result = await models({ view: "installed", includeLibrary: true });
    expect(result.models[0]).toMatchObject({ benchmark: { tokens_per_second: 42 }, policy_rank: { coding: 0 },
      knowledge: { library: { status: "matched" }, supportedFeatures: ["completion", "future-feature"] } });
    expect(requests.filter((url) => url === "/api/tags")).toHaveLength(1);
  });

  it("returns all tag variants with paging and excludes cloud tags from trial recommendations", async () => {
    const first = await models({ view: "library", model: "gemma3", limit: 1 });
    expect(first.tags).toHaveLength(1);
    expect(first.page.total).toBe(3);
    expect(first.recommendation.tag).toBe("gemma3:4b");
    expect(first.recommendation.taskQuality).toBe("unmeasured");
    const next = await models({ view: "library", model: "gemma3", limit: 2, cursor: first.page.next_cursor, includeReadme: true });
    expect(next.tags.map((tag: any) => tag.tag)).toEqual(["gemma3:4b", "gemma3:4b-cloud"]);
    expect(next.tags[1].cloud).toBe(true);
    expect(next.library.readme.format).toBe("markdown");
  });

  it("keeps local metadata when a custom model has no public card", async () => {
    const result = await models({ view: "detail", model: "custom:latest", includeLibrary: true });
    expect(result.knowledge).toMatchObject({ supportedFeatures: ["completion", "future-feature"], library: { status: "not_found" } });
  });

  it("skips public lookup for remote aliases", async () => {
    const result = await models({ view: "detail", model: "remote-alias:latest", includeLibrary: true });
    expect(result.knowledge).toMatchObject({ localModel: false, library: { status: "remote_model" } });
    expect(requests).toEqual(["/api/show"]);
  });

  it.each([
    { view: "raw", includeLibrary: true }, { view: "resident", includeLibrary: true },
    { view: "installed", includeReadme: true }, { view: "detail", model: "gemma3:1b", includeReadme: true },
    { view: "library", includeReadme: true },
  ])("rejects incompatible enrichment flags before any I/O: %j", async (args) => {
    const result = await client.callTool({ name: "models", arguments: { endpoint, ollamaEndpoint: endpoint, ...args } });
    expect(result.isError).toBe(true);
    expect(requests).toHaveLength(0);
  });
});
