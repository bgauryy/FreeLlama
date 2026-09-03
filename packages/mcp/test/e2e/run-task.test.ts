// run_task and requiredCapabilities against a real `freellama serve`. Always starts the current
// release binary on an isolated port: reusing any process on :11435 let a stale installed binary
// pass preview checks, then reject a newer MCP request shape only during generation.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  connectClient,
  type IsolatedServe,
  releaseServeAvailable,
  startIsolatedServe,
} from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe.runIf(releaseServeAvailable)("run_task against a live serve", () => {
  let client: Client;
  let isolated: IsolatedServe | null = null;

  beforeAll(async () => {
    isolated = await startIsolatedServe();
    client = await connectClient({ FREELLAMA_SERVE_ENDPOINT: isolated.endpoint });
  });

  afterAll(async () => {
    await client?.close();
    isolated?.child.kill();
  });

  const call = (name: string, args: Record<string, unknown>, timeout = 180_000) =>
    client.callTool({ name, arguments: args }, undefined, { timeout }) as Promise<ToolResult>;

  it('honors requiredCapabilities: ["vision"] in a preview route', async () => {
    const route = await call("run_task", {
      task: "completion",
      objective: "fastest",
      requiredCapabilities: ["vision"],
      preview: true,
    });
    expect(route.isError ?? false, route.content[0].text).toBe(false);
    expect(route.structuredContent.required_capabilities).toContain("vision");
    expect(route.structuredContent.selected_model).toBeTruthy();
  });

  it('enforces requiredCapabilities: ["audio"] against the installed catalog', async () => {
    const route = await call("run_task", {
      task: "completion",
      objective: "fastest",
      requiredCapabilities: ["audio"],
      preview: true,
    });
    if (route.isError) {
      expect(route.content[0].text).toMatch(/audio|eligible|capabilit/i);
    } else {
      expect(route.structuredContent.required_capabilities).toContain("audio");
      const selectedModel = route.structuredContent.selected_model;
      expect(selectedModel).toBeTruthy();
      const detail = await call("models", { view: "detail", model: selectedModel });
      expect(detail.isError ?? false, detail.content[0].text).toBe(false);
      expect(detail.structuredContent.capabilities).toContain("audio");
    }
  });

  it("actually routes and executes a prompt", async () => {
    const result = await call("run_task", {
      task: "completion",
      objective: "fastest",
      prompt: "Reply with exactly the word: PONG",
      keepAlive: "0",
    });
    expect(result.isError ?? false, result.content[0].text).toBe(false);
    expect(result.structuredContent.route?.selected_model).toBeTruthy();
    expect(result.structuredContent.response?.message).toBeTruthy();
  });
});
