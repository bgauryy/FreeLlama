// task_jobs: follow work submitted with run_task {defer:true}.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { outputInput } from "../tool-results.js";
import { fallbackCheck, resultChecks } from "../result-checks.js";
import { cancelTaskJob, getTaskJob, listTaskJobs, removeTaskJob } from "../serve-client.js";
import {
  endpointParam, errorResult, isStructuredSuccess, jobsText, objectResultSchema, parsedResult, structuredResult, summarizeEmbeddings,
  taskAnswerText,
} from "../helpers.js";

/** A finished job's answer gets the check: lines run_task gives a direct answer (cut off, unsupported facts, unsure decisions). */
function jobChecks(content: Record<string, unknown>): string[] {
  const job = content.job as Record<string, unknown> | undefined;
  const receipt = job?.result;
  if (!job || !receipt || typeof receipt !== "object" || Array.isArray(receipt)) return [];
  const structured = receipt as Record<string, unknown>;
  const answer = (taskAnswerText(structured) ?? "").replace(/^\[[^\]\n]*\] /, "");
  const fallback = fallbackCheck(structured);
  const task = typeof job.task === "string" ? job.task : undefined;
  return [...(fallback ? [fallback] : []), ...resultChecks({ task, answer, structured })];
}

/** The job's status and answer as text, its check: lines after them; the whole receipt stays structuredContent. */
function jobResult(content: Record<string, unknown>) {
  const checks = jobChecks(content);
  const text = [jobsText(content), ...checks.map((check) => `check: ${check}`)].filter(Boolean).join("\n");
  return structuredResult(checks.length > 0 ? { ...content, checks } : content, { text });
}

export function registerTaskJobsTool(server: McpServer): void {
  server.registerTool(
    "task_jobs",
    {
      description:
        "Use when: following run_task {defer:true} work (jobId is its job_id, or one of accepted_job_ids): list, get (result), cancel, or remove (cancels first) a job. " +
        "Do not use when: submitting work or killing a session. Returns: the job's status, then its answer, ranking or error as text, then response_file.",
      inputSchema: z.object({
        endpoint: endpointParam,
        action: z.enum(["list", "get", "cancel", "remove"]),
        jobId: z.string().uuid().optional(),
        returnEmbeddings: z.boolean().optional(),
        output: outputInput.output.describe("get only; as run_task"),
      }).strict(),
      outputSchema: objectResultSchema,
      // One tool for reads and changes, so not read-only (the spec default, not restated). Not destructive either: cancel and remove
      // act only on a job this client submitted (its id), never on other clients' work or on data
      // outside FreeLlama, and a job's input can be submitted again.
      annotations: { destructiveHint: false },
    },
    async ({ endpoint, action, jobId, returnEmbeddings, output }) => {
      try {
        if (action === "list" ? jobId !== undefined : jobId === undefined) {
          return errorResult(new Error("Use jobId only with get/cancel/remove; all require a jobId."));
        }
        if ((returnEmbeddings !== undefined || output !== undefined) && action !== "get") {
          return errorResult(new Error("returnEmbeddings and output are valid only with get."));
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
            if (trimmed) return jobResult({ ...result.structuredContent, job: { ...job, result: trimmed } });
          }
        }
        return isStructuredSuccess(result) ? jobResult(result.structuredContent) : result;
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
