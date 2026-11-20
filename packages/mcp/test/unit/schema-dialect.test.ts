import { describe, expect, it } from "vitest";
import type { JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";
import { normalizeToolSchemas } from "../../src/schema-dialect.js";

const dialect = "http://json-schema.org/draft-07/schema#";

describe("normalizeToolSchemas", () => {
  it("removes the dialect URI from tool input and output schemas only", () => {
    const message = { jsonrpc: "2.0", id: 2, result: { tools: [
      { name: "a", inputSchema: { $schema: dialect, type: "object", properties: { x: { type: "string" } } },
        outputSchema: { $schema: dialect, type: "object" } },
      { name: "b", inputSchema: { type: "object" } },
    ] } } as JSONRPCMessage;
    expect(normalizeToolSchemas(message)).toEqual({ jsonrpc: "2.0", id: 2, result: { tools: [
      { name: "a", inputSchema: { type: "object", properties: { x: { type: "string" } } }, outputSchema: { type: "object" } },
      { name: "b", inputSchema: { type: "object" } },
    ] } });
  });

  it("publishes open objects as additionalProperties: true at every depth and keeps real value schemas", () => {
    const message = { jsonrpc: "2.0", id: 4, result: { tools: [{ name: "c",
      inputSchema: { type: "object", properties: { m: { type: "array", items: { type: "object", additionalProperties: {} } },
        o: { type: "object", additionalProperties: { type: "string" } } } },
      outputSchema: { type: "object", additionalProperties: {} } }] } } as JSONRPCMessage;
    expect(normalizeToolSchemas(message)).toEqual({ jsonrpc: "2.0", id: 4, result: { tools: [{ name: "c",
      inputSchema: { type: "object", properties: { m: { type: "array", items: { type: "object", additionalProperties: true } },
        o: { type: "object", additionalProperties: { type: "string" } } } },
      outputSchema: { type: "object", additionalProperties: true } }] } });
  });

  it("drops what the schema and MCP defaults already imply and keeps real bounds", () => {
    const safe = Number.MAX_SAFE_INTEGER;
    const message = { jsonrpc: "2.0", id: 5, result: { tools: [
      { name: "d", execution: { taskSupport: "forbidden" },
        inputSchema: { type: "object", properties: {
          n: { type: "integer", exclusiveMinimum: 0, maximum: safe }, r: { type: "integer", minimum: 0, maximum: safe },
          i: { type: "integer", minimum: -safe, maximum: safe }, c: { type: "integer", minimum: 1, maximum: 366 },
          m: { type: "object", propertyNames: { type: "string" }, additionalProperties: { type: "string" } },
          p: { type: "object", propertyNames: { pattern: "^a" } } } } },
      { name: "e", execution: { taskSupport: "optional" }, inputSchema: { type: "object" } },
    ] } } as JSONRPCMessage;
    expect(normalizeToolSchemas(message)).toEqual({ jsonrpc: "2.0", id: 5, result: { tools: [
      { name: "d", inputSchema: { type: "object", properties: {
        n: { type: "integer", exclusiveMinimum: 0 }, r: { type: "integer", minimum: 0 }, i: { type: "integer" },
        c: { type: "integer", minimum: 1, maximum: 366 },
        m: { type: "object", additionalProperties: { type: "string" } },
        p: { type: "object", propertyNames: { pattern: "^a" } } } } },
      { name: "e", execution: { taskSupport: "optional" }, inputSchema: { type: "object" } },
    ] } });
  });

  it("leaves every other message unchanged", () => {
    const call = { jsonrpc: "2.0", id: 3, result: { content: [{ type: "text", text: "$schema" }] } } as JSONRPCMessage;
    expect(normalizeToolSchemas(structuredClone(call))).toEqual(call);
  });
});
