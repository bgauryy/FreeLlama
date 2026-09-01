// Basic liveness (tools/list + doctor with no serve required) and proof that
// FREELLAMA_OLLAMA_ENDPOINT actually changes behavior.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe("doctor", () => {
  let client: Client;

  beforeAll(async () => {
    client = await connectClient();
  });

  afterAll(async () => {
    await client.close();
  });

  it("lists the expected tools", async () => {
    const { tools } = await client.listTools();
    const names = tools.map((tool) => tool.name);
    for (const expected of ["doctor", "models", "run_task", "ollama_manage", "ollama_delete", "delegate_research"]) {
      expect(names).toContain(expected);
    }
  });

  it("succeeds against the live Ollama", async () => {
    const result = (await client.callTool({ name: "doctor", arguments: {} })) as ToolResult;
    expect(result.isError ?? false).toBe(false);
    expect(typeof result.structuredContent?.endpoint).toBe("string");
  });
});

describe("FREELLAMA_OLLAMA_ENDPOINT override", () => {
  it("changes which endpoint doctor talks to", async () => {
    // Point at a port nothing is listening on to prove the env var actually changes behavior:
    // the exact same call succeeds against the real endpoint (asserted above), so an error here
    // can only mean the override was honored.
    const client = await connectClient({ FREELLAMA_OLLAMA_ENDPOINT: "http://127.0.0.1:9999" });
    try {
      const result = (await client.callTool({ name: "doctor", arguments: {} })) as ToolResult;
      expect(result.isError).toBe(true);
      expect(result.content[0].text).toMatch(/connect|unreachable/i);
    } finally {
      await client.close();
    }
  });
});
