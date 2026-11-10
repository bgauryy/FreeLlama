// Every FreeLlama MCP tool, callable in-process: each `register*Tool(server)` receives a stub whose
// `registerTool` keeps the tool's config and handler (as the unit tests do). The CLI and gateway
// therefore run the exact MCP handlers with the exact MCP input schemas; nothing is reimplemented.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import type { ToolExtra } from "../progress.js";
import { TOOL_REGISTRATIONS } from "../tools/registrations.js";
import { UsageError } from "./args.js";

export type ToolResult = {
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
};
type Handler = (args: Record<string, unknown>, extra: ToolExtra) => Promise<ToolResult>;
type RegisteredTool = { name: string; description: string; inputSchema: z.ZodType; handler: Handler };

let tools: Map<string, RegisteredTool> | undefined;

/** The registered tools, in MCP's tools/list order. */
export function toolRegistry(): Map<string, RegisteredTool> {
  if (tools) return tools;
  const found = new Map<string, RegisteredTool>();
  const stub = {
    registerTool(name: string, config: { description?: string; inputSchema?: unknown }, handler: Handler) {
      const schema = config.inputSchema;
      // The SDK accepts a Zod object or a raw shape; normalize to one schema to parse with.
      const inputSchema = schema instanceof z.ZodType ? schema : z.object((schema ?? {}) as z.ZodRawShape);
      found.set(name, { name, description: config.description ?? "", inputSchema, handler });
    },
  } as unknown as McpServer;
  for (const register of TOOL_REGISTRATIONS) register(stub);
  tools = found;
  return found;
}

export type CallOptions = {
  signal?: AbortSignal;
  /** Receives progress and notice lines; omitted, the call is silent. */
  onMessage?: (level: "progress" | "info" | "warning", message: string) => void;
};

/** An MCP `extra` for an in-process call: cancellation from `signal`, notifications to `onMessage`. */
function toolExtra(options: CallOptions = {}): ToolExtra {
  const { onMessage } = options;
  return {
    signal: options.signal ?? new AbortController().signal,
    ...(onMessage ? { _meta: { progressToken: "freellama" } } : {}),
    sendNotification: async (notification) => {
      if (!onMessage) return;
      if (notification.method === "notifications/progress") onMessage("progress", notification.params.message ?? "");
      else onMessage(notification.params.level, notification.params.data);
    },
  };
}

/** Validate `args` with the tool's own schema (a refusal is a UsageError) and run its handler. */
export async function callTool(name: string, args: Record<string, unknown>, options: CallOptions = {}): Promise<ToolResult> {
  const tool = toolRegistry().get(name);
  if (!tool) throw new UsageError(`unknown tool "${name}"; \`freellama tools\` lists them.`);
  const parsed = tool.inputSchema.safeParse(args);
  if (!parsed.success) {
    throw new UsageError(`${name}: ${parsed.error.issues.map((issue) => `${issue.path.length ? `${issue.path.join(".")}: ` : ""}${issue.message}`).join("; ")}`);
  }
  return tool.handler(parsed.data as Record<string, unknown>, toolExtra(options));
}

/** The text a tool result shows, joined. */
export function resultText(result: ToolResult): string {
  return (result.content ?? []).map((part) => part.text ?? "").filter(Boolean).join("\n");
}
