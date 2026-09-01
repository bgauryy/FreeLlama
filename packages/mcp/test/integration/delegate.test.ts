// delegate_research guardrails that need the real server but never spawn a model:
// the workspace boundary and the pre-flight refusal of a measured-unusable model.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient, REPO_ROOT } from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe("delegate_research guardrails", () => {
  let client: Client;

  beforeAll(async () => {
    client = await connectClient();
  });

  afterAll(async () => {
    await client.close();
  });

  it("refuses a workspace outside the allowed roots (/etc)", async () => {
    const escaped = (await client.callTool({
      name: "delegate_research",
      arguments: { question: "list files", workspacePath: "/etc", model: "qwen2.5:0.5b" },
    })) as ToolResult;
    expect(escaped.isError).toBe(true);
    expect(escaped.content[0].text).toMatch(/outside the allowed|no research roots/i);
  });

  it("refuses an unusable model before spawning anything, as a verdict not an error", async () => {
    const started = Date.now();
    const weak = (await client.callTool({
      name: "delegate_research",
      arguments: {
        question: "In packages/rust-core/Cargo.toml, what optional feature enables the Node addon?",
        workspacePath: REPO_ROOT,
        model: "qwen2.5:0.5b",
      },
    })) as ToolResult;
    const elapsed = Date.now() - started;

    expect(weak.isError ?? false).toBe(false);
    const verification = weak.structuredContent.verification;
    expect(verification.recommendation).toBe("escalate");
    // The base rate must name the model that was asked for, not quote another model's number.
    expect(verification.measuredBaseRate).toMatch(/qwen2\.5:0\.5b/);
    expect(verification.measuredBaseRate).not.toMatch(/^98\.9/);
    expect(weak.structuredContent.toolCallCount).toBe(0);
    // Fired before generation, or the pre-flight saved nothing.
    expect(elapsed).toBeLessThan(5000);
  });
});
