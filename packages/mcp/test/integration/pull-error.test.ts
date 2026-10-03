// A pull stream can report an error after HTTP 200. Exercise the real NDJSON parser
// against a disposable fixture; no registry request or model download is required.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe("ollama_manage pull failure", () => {
  let client: Client;
  let server: Server;
  const requests: { url?: string; method?: string; body: unknown }[] = [];

  beforeAll(async () => {
    server = createServer(async (request, response) => {
      let body = "";
      for await (const chunk of request) body += String(chunk);
      requests.push({ url: request.url, method: request.method, body: JSON.parse(body) });
      response.writeHead(200, { "content-type": "application/x-ndjson" });
      response.write(`${JSON.stringify({ status: "pulling manifest" })}\n`);
      response.end(`${JSON.stringify({ error: "fixture manifest not found" })}\n`);
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

  it("reports a streamed error after progress, not a success-shaped payload", async () => {
    const result = (await client.callTool(
      { name: "ollama_manage", arguments: { action: "pull", model: "pull-error:fixture" } },
      undefined,
      { timeout: 5000 },
    )) as ToolResult;
    expect(requests).toEqual([{ url: "/api/pull", method: "POST", body: { model: "pull-error:fixture", stream: true } }]);
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toContain("fixture manifest not found");
    expect(result.structuredContent).toBeUndefined();
  });
});
