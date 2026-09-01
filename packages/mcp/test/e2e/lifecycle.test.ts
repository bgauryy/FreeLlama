// ollama_manage pull → verify → ollama_delete round trip, asserted net-zero against the
// installed-model list. Uses the smallest real tag (~400MB download on first run). Skipped when
// the tag is already installed: deleting a model the user actually has is not a test's call.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

const TAG = "qwen2.5:0.5b";

describe("ollama_manage / ollama_delete lifecycle", () => {
  let client: Client;
  let before: string[];

  beforeAll(async () => {
    client = await connectClient();
    before = await installed();
  });

  afterAll(async () => {
    await client.close();
  });

  const call = (name: string, args: Record<string, unknown>, timeout = 600_000) =>
    client.callTool({ name, arguments: args }, undefined, { timeout }) as Promise<ToolResult>;

  async function installed(): Promise<string[]> {
    const result = await call("models", { view: "raw" }, 30_000);
    return (result.structuredContent.models ?? []).map((model: { name: string }) => model.name);
  }

  it("pulls, verifies, deletes, and lands net-zero", async (ctx) => {
    if (before.includes(TAG)) {
      ctx.skip(`${TAG} is already installed — refusing to delete a model the user has`);
    }

    const pull = await call("ollama_manage", { action: "pull", model: TAG });
    expect(pull.isError ?? false, pull.content[0].text).toBe(false);
    expect(await installed()).toContain(TAG);

    const del = await call("ollama_delete", { model: TAG }, 60_000);
    expect(del.isError ?? false, del.content[0].text).toBe(false);

    const after = await installed();
    expect(after).not.toContain(TAG);
    expect(after.sort()).toEqual([...before].sort());
  });
});
