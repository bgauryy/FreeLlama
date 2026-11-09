// Pull approval over the real MCP protocol, against a disposable Ollama fixture: the deprecated
// ollama_manage pull downloads nothing, and ollama_pull refuses a tag whose size it cannot match
// to an approval. No model is downloaded. (The streamed-error parser is covered by unit tests.)
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe("pull approval", () => {
  let client: Client;
  let server: Server;
  const requests: { url?: string; method?: string }[] = [];

  beforeAll(async () => {
    server = createServer(async (request, response) => {
      for await (const _chunk of request) { /* drain */ }
      requests.push({ url: request.url, method: request.method });
      if (request.url === "/api/tags") {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify({ models: [] }));
        return;
      }
      response.writeHead(200, { "content-type": "application/x-ndjson" });
      response.end(`${JSON.stringify({ error: "fixture: pull must not be reached" })}\n`);
    });
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("fixture listener missing");
    client = await connectClient({ FREELLAMA_OLLAMA_ENDPOINT: `http://127.0.0.1:${address.port}` });
  });

  afterAll(async () => {
    await client?.close();
    if (server?.listening) await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });

  it("refuses the deprecated ollama_manage pull without contacting Ollama", async () => {
    const result = (await client.callTool({ name: "ollama_manage", arguments: { action: "pull", model: "pull-error:fixture" } },
      undefined, { timeout: 5000 })) as ToolResult;
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toMatch(/ollama_pull/);
    expect(requests.filter((request) => request.url === "/api/pull")).toEqual([]);
  });

  it("refuses a tag the library cannot size, without pulling", async () => {
    const result = (await client.callTool({ name: "ollama_pull", arguments: { model: "pull-error-fixture-nonexistent:fixture", approvedSizeBytes: 1000 } },
      undefined, { timeout: 30_000 })) as ToolResult;
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toMatch(/^Not pulled|Not pulled:/);
    expect(requests.filter((request) => request.url === "/api/pull")).toEqual([]);
  });
});
