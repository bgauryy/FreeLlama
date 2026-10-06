import { describe, expect, it } from "vitest";
import type { JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";
import { stripSchemaDialect } from "../../src/schema-dialect.js";

const dialect = "http://json-schema.org/draft-07/schema#";

describe("stripSchemaDialect", () => {
  it("removes the dialect URI from tool input and output schemas only", () => {
    const message = { jsonrpc: "2.0", id: 2, result: { tools: [
      { name: "a", inputSchema: { $schema: dialect, type: "object", properties: { x: { type: "string" } } },
        outputSchema: { $schema: dialect, type: "object" } },
      { name: "b", inputSchema: { type: "object" } },
    ] } } as JSONRPCMessage;
    expect(stripSchemaDialect(message)).toEqual({ jsonrpc: "2.0", id: 2, result: { tools: [
      { name: "a", inputSchema: { type: "object", properties: { x: { type: "string" } } }, outputSchema: { type: "object" } },
      { name: "b", inputSchema: { type: "object" } },
    ] } });
  });

  it("leaves every other message unchanged", () => {
    const call = { jsonrpc: "2.0", id: 3, result: { content: [{ type: "text", text: "$schema" }] } } as JSONRPCMessage;
    expect(stripSchemaDialect(structuredClone(call))).toEqual(call);
  });
});
