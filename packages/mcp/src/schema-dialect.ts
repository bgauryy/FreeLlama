import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";

/**
 * Remove the `$schema` dialect URI the SDK's zod converter puts on every tool input and output
 * schema. MCP fixes the dialect, so the URI only costs tokens on each tools/list (18 copies).
 */
export function stripSchemaDialect(message: JSONRPCMessage): JSONRPCMessage {
  const tools = (message as { result?: { tools?: unknown } }).result?.tools;
  if (!Array.isArray(tools)) return message;
  for (const tool of tools as Record<string, Record<string, unknown> | undefined>[]) {
    delete tool.inputSchema?.$schema;
    delete tool.outputSchema?.$schema;
  }
  return message;
}

export function withoutSchemaDialect<T extends Transport>(transport: T): T {
  const send = transport.send.bind(transport);
  transport.send = (message, options) => send(stripSchemaDialect(message), options);
  return transport;
}
