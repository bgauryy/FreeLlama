// A failed pull must be an error result. Ollama reports pull failures as an {"error": ...} NDJSON
// event on an HTTP 200 stream, so nothing at the transport layer fails — the server has to read
// the stream. Safe to run anywhere: the tag cannot exist, so nothing downloads.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe("ollama_manage pull failure", () => {
  let client: Client;

  beforeAll(async () => {
    client = await connectClient();
  });

  afterAll(async () => {
    await client.close();
  });

  it("reports a nonexistent tag as an error, not a success-shaped payload", async () => {
    const result = (await client.callTool(
      {
        name: "ollama_manage",
        arguments: { action: "pull", model: "definitely-not-a-real-model-xyz:1b" },
      },
      undefined,
      { timeout: 60_000 },
    )) as ToolResult;
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toMatch(/file does not exist|pull model manifest|not found/i);
  });
});
