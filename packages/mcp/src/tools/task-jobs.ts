// task_jobs: follow work submitted with run_task {defer:true}.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { cancelTaskJob, getTaskJob, listTaskJobs, removeTaskJob } from "../serve-client.js";
import {
  endpointParam, errorResult, isStructuredSuccess, objectResultSchema, parsedResult, structuredResult, summarizeEmbeddings,
} from "../helpers.js";

export function registerTaskJobsTool(server: McpServer): void {
  server.registerTool(
    "task_jobs",
    {
      description:
        "Use when: following work submitted with run_task {defer:true}: list, get (result), cancel, or remove a job. " +
        "Do not use when: submitting new work or killing a whole session. Returns: job status/result; remove cancels first.",
      inputSchema: z.object({
        endpoint: endpointParam,
        action: z.enum(["list", "get", "cancel", "remove"]),
        jobId: z.string().uuid().optional(),
        returnEmbeddings: z.boolean().optional(),
      }).strict(),
      outputSchema: objectResultSchema,
      annotations: { destructiveHint: true },
    },
    async ({ endpoint, action, jobId, returnEmbeddings }) => {
      try {
        if (action === "list" ? jobId !== undefined : jobId === undefined) {
          return errorResult(new Error("Use jobId only with get/cancel/remove; all require a jobId."));
        }
        if (returnEmbeddings !== undefined && action !== "get") {
          return errorResult(new Error("returnEmbeddings is valid only with get."));
        }
        const result = parsedResult(action === "list"
          ? await listTaskJobs(endpoint)
          : action === "get"
            ? await getTaskJob(endpoint, jobId!)
            : action === "cancel"
              ? await cancelTaskJob(endpoint, jobId!)
              : await removeTaskJob(endpoint, jobId!));
        if (!returnEmbeddings && isStructuredSuccess(result)) {
          const job = result.structuredContent.job as Record<string, unknown> | undefined;
          if (job?.result && typeof job.result === "object") {
            const trimmed = summarizeEmbeddings(job.result as Record<string, unknown>);
            if (trimmed) return structuredResult({ ...result.structuredContent, job: { ...job, result: trimmed } });
          }
        }
        return result;
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
