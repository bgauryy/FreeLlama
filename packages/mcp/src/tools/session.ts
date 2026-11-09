// session: managed affinity sessions (create/delete/kill).
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { createSession, deleteSession, killSession } from "../serve-client.js";
import { endpointParam, errorResult, parsedResult, structuredResult } from "../helpers.js";

const sessionResultSchema = z.object({
  session_id: z.string().optional(), deleted: z.boolean().optional(),
  killed: z.boolean().optional(), runner_stop_confirmed: z.boolean().optional(),
}).loose();

export function registerSessionTool(server: McpServer): void {
  server.registerTool(
    "session",
    {
      description:
        "Use when: related run_task calls should stay on one model with a warm runner: create, then pass sessionId; kill stops its running tasks, delete forgets it. " +
        "Do not use when: cancelling one deferred job (task_jobs), keeping history (scope) or freeing memory (ollama_manage unload). Returns: a receipt.",
      inputSchema: z.object({
        action: z.enum(["create", "delete", "kill"]),
        sessionId: z.string().uuid().optional(),
        endpoint: endpointParam,
      }).strict(),
      outputSchema: sessionResultSchema,
      // kill and delete act only on a session this client created (its id): they stop that
      // session's own tasks and forget it, never other clients' work or anything outside FreeLlama.
      annotations: { destructiveHint: false },
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
