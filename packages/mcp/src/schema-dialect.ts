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

const MAX_SAFE = Number.MAX_SAFE_INTEGER;

/**
 * Drop what the JSON Schema or MCP defaults already say: `propertyNames: {type: "string"}` (object
 * keys are always strings) and the safe-integer bounds zod writes for every `.int()` (a JSON number
 * past them is not exact anyway; the server still validates). Other bounds stay.
 */
function dropImpliedKeywords(schema: unknown): void {
  if (Array.isArray(schema)) {
    for (const item of schema) dropImpliedKeywords(item);
    return;
  }
  if (!schema || typeof schema !== "object") return;
  const node = schema as Record<string, unknown>;
  const names = node.propertyNames;
  if (names && typeof names === "object" && JSON.stringify(names) === '{"type":"string"}') delete node.propertyNames;
  if (node.type === "integer") {
    if (node.maximum === MAX_SAFE) delete node.maximum;
    if (node.minimum === -MAX_SAFE) delete node.minimum;
  }
  for (const value of Object.values(node)) dropImpliedKeywords(value);
}

/**
 * Normalize every tool in a tools/list answer: drop the `$schema` dialect URI the SDK's zod
 * converter adds (MCP fixes the dialect, so it only costs tokens on each tools/list), publish open
 * objects as `additionalProperties: true`, the form clients have always received, drop the
 * keywords the defaults imply, and omit `execution.taskSupport: "forbidden"` (the MCP default).
 */
export function normalizeToolSchemas(message: JSONRPCMessage): JSONRPCMessage {
  const tools = (message as { result?: { tools?: unknown } }).result?.tools;
  if (!Array.isArray(tools)) return message;
  for (const tool of tools as Record<string, Record<string, unknown> | undefined>[]) {
    for (const schema of [tool.inputSchema, tool.outputSchema]) {
      if (!schema) continue;
      delete schema.$schema;
      openObjectsAsTrue(schema);
      dropImpliedKeywords(schema);
    }
    const execution = tool.execution;
    if (execution?.taskSupport === "forbidden") {
      delete execution.taskSupport;
      if (Object.keys(execution).length === 0) delete tool.execution;
    }
  }
  return message;
}

export function withNormalizedToolSchemas<T extends Transport>(transport: T): T {
  const send = transport.send.bind(transport);
  transport.send = (message, options) => send(normalizeToolSchemas(message), options);
  return transport;
}
