// task_jobs: follow work submitted with run_task {defer:true}.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { cancelTaskJob, getTaskJob, listTaskJobs, removeTaskJob } from "../serve-client.js";
import {
  endpointParam, errorResult, isStructuredSuccess, jobsText, objectResultSchema, parsedResult, structuredResult, summarizeEmbeddings,
} from "../helpers.js";

export function registerTaskJobsTool(server: McpServer): void {
  server.registerTool(
    "task_jobs",
    {
      description:
        "Use when: following run_task {defer:true} work: list, get (result), cancel, or remove (cancels first) a job. " +
        "Do not use when: submitting work or killing a session. Returns: the job's status, then its answer, ranking or error as text (full receipt in structuredContent).",
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
            if (trimmed) {
              const content = { ...result.structuredContent, job: { ...job, result: trimmed } };
              return structuredResult(content, { text: jobsText(content) });
            }
          }
        }
        // The text is the job's status and its answer; the whole receipt is structuredContent.
        return isStructuredSuccess(result) ? structuredResult(result.structuredContent, { text: jobsText(result.structuredContent) }) : result;
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
