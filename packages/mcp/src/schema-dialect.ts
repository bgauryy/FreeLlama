import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";

/** `additionalProperties: {}` (any value, as zod 4 writes it) becomes `true`, through every nested schema. */
function openObjectsAsTrue(schema: unknown): void {
  if (Array.isArray(schema)) {
    for (const item of schema) openObjectsAsTrue(item);
    return;
  }
  if (!schema || typeof schema !== "object") return;
  const node = schema as Record<string, unknown>;
  const extra = node.additionalProperties;
  if (extra && typeof extra === "object" && !Array.isArray(extra) && Object.keys(extra).length === 0) {
    node.additionalProperties = true;
  }
  for (const value of Object.values(node)) openObjectsAsTrue(value);
}

/**
 * Normalize every tool schema in a tools/list answer: drop the `$schema` dialect URI the SDK's zod
 * converter adds (MCP fixes the dialect, so it only costs tokens on each tools/list), and publish
 * open objects as `additionalProperties: true`, the form clients have always received.
 */
export function normalizeToolSchemas(message: JSONRPCMessage): JSONRPCMessage {
  const tools = (message as { result?: { tools?: unknown } }).result?.tools;
  if (!Array.isArray(tools)) return message;
  for (const tool of tools as Record<string, Record<string, unknown> | undefined>[]) {
    for (const schema of [tool.inputSchema, tool.outputSchema]) {
      if (!schema) continue;
      delete schema.$schema;
      openObjectsAsTrue(schema);
    }
  }
  return message;
}

export function withNormalizedToolSchemas<T extends Transport>(transport: T): T {
  const send = transport.send.bind(transport);
  transport.send = (message, options) => send(normalizeToolSchemas(message), options);
  return transport;
}
