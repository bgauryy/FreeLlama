// ollama_manage pull → verify → ollama_delete round trip, asserted net-zero against the
// installed-model list. Uses the smallest real tag (~400MB download on first run). Skipped when
// the tag is already installed: deleting a model the user actually has is not a test's call.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

const TAG = "qwen2.5:0.5b";

// Running the suite is not approval to download or delete a model. The operator must approve
// this exact tag and the current reported byte size separately.
describe.runIf(process.env.FREELLAMA_TEST_PULL_TAG === TAG && !!process.env.FREELLAMA_TEST_PULL_SIZE_BYTES)("ollama_manage / ollama_delete lifecycle", () => {
  let client: Client;
  let before: string[];

  beforeAll(async () => {
    client = await connectClient();
    before = await installed();
  });

  afterAll(async () => {
    await client?.close();
  });

  const call = (name: string, args: Record<string, unknown>, timeout = 600_000) =>
    client.callTool({ name, arguments: args }, undefined, { timeout }) as Promise<ToolResult>;

  async function installed(): Promise<string[]> {
    const names: string[] = [];
    let cursor: string | undefined;
    do {
      const result = await call("models", { view: "raw", ...(cursor ? { cursor } : {}) }, 30_000);
      expect(result.isError ?? false, result.content[0].text).toBe(false);
      names.push(...(result.structuredContent.models ?? []).map((model: { name: string }) => model.name));
      cursor = result.structuredContent.page?.next_cursor ?? undefined;
    } while (cursor);
    return names;
  }

  it("pulls, verifies, deletes, and lands net-zero", async (ctx) => {
    if (before.includes(TAG)) {
      ctx.skip(`${TAG} is already installed — refusing to delete a model the user has`);
    }

    let tag: { tag: string; sizeBytes?: number } | undefined;
    let cursor: string | undefined;
    do {
      const library = await call("models", { view: "library", model: TAG, limit: 50, ...(cursor ? { cursor } : {}) });
      expect(library.isError ?? false, library.content[0].text).toBe(false);
      tag = library.structuredContent.tags?.find((entry: { tag: string }) => entry.tag === TAG);
      cursor = library.structuredContent.page?.next_cursor ?? undefined;
    } while (!tag && cursor);
    expect(tag?.sizeBytes, "exact tag and current reported download size must be available").toBeGreaterThan(0);
    expect(String(tag?.sizeBytes), "approved size changed; obtain fresh exact-tag approval").toBe(process.env.FREELLAMA_TEST_PULL_SIZE_BYTES);

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
