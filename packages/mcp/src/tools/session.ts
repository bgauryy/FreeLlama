// session: managed affinity sessions (create/delete/kill).
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { createSession, deleteSession, killSession } from "../serve-client.js";
import { endpointParam, errorResult, parsedResult, structuredResult } from "../helpers.js";

const sessionResultSchema = z.object({
  session_id: z.string().optional(), deleted: z.boolean().optional(),
  killed: z.boolean().optional(), runner_stop_confirmed: z.boolean().optional(),
}).passthrough();

export function registerSessionTool(server: McpServer): void {
  server.registerTool(
    "session",
    {
      description:
        "Use when: several related run_task calls should stay on one model (create, pass sessionId), or to kill a session's in-flight work. " +
        "Do not use when: cancelling one deferred job (task_jobs), keeping history (scope), or unloading (ollama_manage). Returns: receipt.",
      inputSchema: z.object({
        action: z.enum(["create", "delete", "kill"]),
        sessionId: z.string().uuid().optional(),
        endpoint: endpointParam,
      }).strict(),
      outputSchema: sessionResultSchema,
      annotations: { destructiveHint: true },
    },
    async ({ action, sessionId, endpoint }) => {
      try {
        if (action === "create") {
          if (sessionId !== undefined) return errorResult(new Error("sessionId is only valid for action: delete or kill."));
          return structuredResult(JSON.parse(await createSession(endpoint)));
        }
        if (sessionId === undefined) return errorResult(new Error(`action: ${action} requires sessionId.`));
        if (action === "kill") return parsedResult(await killSession(endpoint, sessionId));
        await deleteSession(endpoint, sessionId);
        return structuredResult({ session_id: sessionId, deleted: true });
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
