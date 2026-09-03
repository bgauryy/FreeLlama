import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

describe("packaged documentation resources", () => {
  let client: Client;

  beforeAll(async () => {
    client = await connectClient();
  });

  afterAll(async () => {
    await client.close();
  });

  it("lists a compact index and every root documentation page", async () => {
    const listed = await client.listResources();
    const uris = listed.resources.map((resource) => resource.uri);
    expect(uris).toContain("freellama://docs/index");
    expect(uris).toContain("freellama://docs/PRODUCTION");
    expect(uris).toContain("freellama://docs/CLI");
    expect(uris).toContain("freellama://docs/OLLAMA_SYSTEM_OPTIMIZATION");
  });

  it("serves documentation lazily through the MCP resource protocol", async () => {
    const index = await client.readResource({ uri: "freellama://docs/index" });
    expect(index.contents).toHaveLength(1);
    expect(index.contents[0].mimeType).toBe("text/markdown");
    expect("text" in index.contents[0]).toBe(true);
    if (!("text" in index.contents[0])) throw new Error("documentation index was not text");
    expect(index.contents[0].text).toContain("FreeLlama packaged documentation");

    const production = await client.readResource({ uri: "freellama://docs/PRODUCTION" });
    expect("text" in production.contents[0]).toBe(true);
    if (!("text" in production.contents[0])) throw new Error("production guide was not text");
    expect(production.contents[0].text).toContain("Configure Ollama explicitly");
  });
});
