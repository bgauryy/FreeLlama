import { describe, expect, it } from "vitest";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { LoggingMessageNotificationSchema, type LoggingLevel } from "@modelcontextprotocol/sdk/types.js";
import { installLogLevelFilter, notice } from "../../src/progress.js";

/** A server with one tool that sends an info and a warning notice, and a client that records them. */
async function connect() {
  const server = new McpServer({ name: "t", version: "1" }, { capabilities: { logging: {} } });
  const installed = installLogLevelFilter(server);
  server.registerTool("notify", { description: "d" }, async (extra) => {
    notice(extra, "info", "loaded m");
    notice(extra, "warning", "waiting for memory");
    return { content: [{ type: "text", text: "ok" }] };
  });
  const client = new Client({ name: "c", version: "1" });
  const received: string[] = [];
  client.setNotificationHandler(LoggingMessageNotificationSchema, (n) => { received.push(String(n.params.data)); });
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  await Promise.all([server.connect(serverSide), client.connect(clientSide)]);
  const call = async (level?: LoggingLevel) => {
    if (level) await client.setLoggingLevel(level);
    received.length = 0;
    await client.callTool({ name: "notify", arguments: {} });
    // Notifications are sent without awaiting; let the in-memory transport deliver them.
    await new Promise((resolve) => setTimeout(resolve, 10));
    return [...received];
  };
  return { installed, call, close: () => Promise.all([client.close(), server.close()]) };
}

describe("log notices and logging/setLevel", () => {
  it("sends every notice before the client sets a level, and only those at or above it after", async () => {
    const { installed, call, close } = await connect();
    expect(installed).toBe(true);
    expect(await call()).toEqual(["loaded m", "waiting for memory"]);
    expect(await call("warning")).toEqual(["waiting for memory"]);
    expect(await call("error")).toEqual([]);
    await close();
  });
});
