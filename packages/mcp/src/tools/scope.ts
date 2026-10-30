// scope: server-side message history (create/get/fork/delete); residency is `run_task {warm:true}`.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { createScope, deleteScope, forkScope, getScope } from "../serve-client.js";
import { endpointParam, errorResult, messageParam, objectResultSchema, parsedResult, structuredResult } from "../helpers.js";

// Routing is set per run_task call and history bounds are operator configuration, so the MCP tool
// exposes neither `route_defaults` nor `limits`.
export function registerScopeTool(server: McpServer): void {
  server.registerTool("scope", {
    description: "Use when: a multi-turn local conversation should keep its message history server-side (create/get/fork/delete), then continue with run_task {scopeId, scopeRevision}. Do not use when: you want model affinity (session) or runner KV control. Returns: process-local scope metadata; message history only with includeMessages:true.",
    inputSchema: z.object({
      action: z.enum(["create", "get", "fork", "delete"]), endpoint: endpointParam,
      scopeId: z.string().uuid().optional(), revision: z.number().int().nonnegative().optional(),
      messages: z.array(messageParam).optional(), includeMessages: z.boolean().optional(),
    }).strict(),
    outputSchema: objectResultSchema, annotations: { destructiveHint: true },
  }, async ({ action, endpoint, scopeId, revision, messages, includeMessages }) => {
    try {
      if ((action === "create") !== (scopeId === undefined)) throw new Error("scopeId is required for get/fork/delete, forbidden for create.");
      if ((action === "fork") !== (revision !== undefined)) throw new Error("revision is required only for fork.");
      if (includeMessages !== undefined && action !== "get") throw new Error("includeMessages is valid only for get.");
      if (messages !== undefined && action !== "create") throw new Error("messages is valid only for create.");
      if (action === "delete") {
        await deleteScope(endpoint, scopeId!);
        return structuredResult({ deleted: true, scope_id: scopeId });
      }
      return parsedResult(action === "create"
        ? await createScope(endpoint, { messages })
        : action === "fork"
          ? await forkScope(endpoint, scopeId!, { revision })
          : await getScope(endpoint, scopeId!, includeMessages));
    } catch (error) { return errorResult(error); }
  });
}
