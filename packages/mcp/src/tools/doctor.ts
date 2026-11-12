// doctor: Ollama health plus optional managed-serve views.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { USER_WORKFLOW_ERRORS } from "../local-prompts.js";
import { doctor, health, machine, usage, status } from "../native.js";
import { doctorFullText, statusText, usageText, withText } from "../result-text.js";
import {
  endpointParam, errorResult, isStructuredSuccess, ollamaEndpointParam, parsedResult, structuredResult,
} from "../helpers.js";

const doctorResultSchema = z.object({
  summary: z.string().optional(), endpoint: z.string().optional(),
}).loose();

type Fields = Record<string, unknown>;

/**
 * serve's `/ready` as reported: `{ready, ...}` from 200 or 503, `supported:false` from a server
 * without the route, the HTTP status for anything else, or why it could not be asked.
 */
export async function serveReadiness(endpoint: string | undefined): Promise<Fields> {
  const base = (endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "");
  try {
    const response = await fetch(`${base}/_freellama/v1/ready`, { signal: AbortSignal.timeout(5_000) });
    if (response.status === 404) return { supported: false, note: "this serve predates /ready; platform_health shows liveness" };
    const body = await response.json().catch(() => undefined) as Fields | undefined;
    if ((response.status === 200 || response.status === 503) && body && typeof body.ready === "boolean") return body;
    return { http_status: response.status };
  } catch (error) {
    return { unreachable: error instanceof Error ? error.message : String(error) };
  }
}

/** `ok` only when Ollama answered (doctor ran) and serve is live and ready; otherwise the reasons. */
export function doctorStatus(report: Fields): { status: "ok" | "degraded"; problems: string[] } {
  const problems: string[] = [];
  const health = report.platform_health as Fields | undefined;
  if (typeof report.platform_health_unavailable === "string") problems.push(report.platform_health_unavailable);
  else if (health && health.status !== "ok") problems.push(`freellama serve health: ${String(health.status)}`);
  const ready = report.readiness as Fields | undefined;
  if (ready?.ready === false) {
    problems.push(`freellama serve not ready: waiting for ${String(ready.waiting_for ?? "startup")}${typeof ready.detail === "string" ? ` (${ready.detail})` : ""}`);
  }
  // A workflow file the user wrote but FreeLlama skipped would otherwise fail silently.
  for (const skipped of USER_WORKFLOW_ERRORS) problems.push(`user workflow skipped: ${skipped.path}: ${skipped.error}`);
  return { status: problems.length > 0 ? "degraded" : "ok", problems };
}

/** "Ollama unreachable on backend gpu (…)" for each backend whose residency query failed. */
/**
 * The status view's jobs as counts by state plus the jobs still active: the full list (often half
 * of the status payload) is task_jobs{action:"list"}'s to give, not every status call's.
 */
export function jobsSummary(value: unknown): Fields | unknown {
  const jobs = value && typeof value === "object" && Array.isArray((value as Fields).jobs) ? (value as Fields).jobs as Fields[] : undefined;
  if (!jobs) return value;
  const terminal = new Set(["completed", "failed", "cancelled", "expired"]);
  const counts: Record<string, number> = {};
  for (const job of jobs) counts[String(job.status ?? "unknown")] = (counts[String(job.status ?? "unknown")] ?? 0) + 1;
  return { ...(value as Fields), jobs: jobs.filter((job) => !terminal.has(String(job.status))), counts, note: 'finished jobs are counted only; task_jobs{action:"list"} lists them' };
}

export function ollamaProblems(status: Fields): string[] {
  const loaded = Array.isArray(status.loaded_models) ? status.loaded_models : [];
  return loaded.flatMap((entry) => {
    const row = entry as Fields;
    return typeof row?.error === "string"
      ? [`Ollama unreachable on backend ${String(row.backend ?? "?")} (${row.error}); ask the user to start Ollama`]
      : [];
  });
}

export function registerDoctorTool(server: McpServer): void {
  server.registerTool(
    "doctor",
    {
      description: "Use when: checking Ollama/FreeLlama health, live queues and limits (view:status) or token usage (view:usage). Do not use when: choosing a model (models). Returns: a short summary; a view gives detail.",
      inputSchema: z.object({
        ollamaEndpoint: ollamaEndpointParam,
        endpoint: endpointParam,
        // The older name; `endpoint` matches every other tool. Both name the serve endpoint.
        serveEndpoint: endpointParam.describe("alias of endpoint"),
        view: z.enum(["summary", "full", "status", "usage"]).optional(),
        days: z.number().int().min(1).max(366).optional(),
      }).strict(),
      outputSchema: doctorResultSchema,
      annotations: { readOnlyHint: true },
    },
    async ({ ollamaEndpoint, endpoint, serveEndpoint: serveAlias, view, days }) => {
      try {
        if (endpoint !== undefined && serveAlias !== undefined && endpoint !== serveAlias) {
          return errorResult(new Error("`endpoint` and `serveEndpoint` name the same serve endpoint; pass one."));
        }
        const serveEndpoint = endpoint ?? serveAlias;
        if (days !== undefined && view !== "usage") return errorResult(new Error("days is only valid with view: usage."));
        // Live views come from serve alone; they do not need the Ollama half of the diagnostic.
        if (view === "status") {
          const live = parsedResult(await status(serveEndpoint));
          // serve's own `status` is its liveness; a backend whose /api/ps failed means Ollama is
          // down, which is what decides whether a task can run.
          if (isStructuredSuccess(live)) {
            const problems = ollamaProblems(live.structuredContent);
            if (problems.length > 0) {
              live.structuredContent.status = "degraded";
              live.structuredContent.problems = problems;
            }
            live.structuredContent.task_jobs = jobsSummary(live.structuredContent.task_jobs);
          }
          return withText(live, statusText);
        }
        if (view === "usage") return withText(parsedResult(await usage(serveEndpoint, days)), usageText);
        const report = parsedResult(await doctor(ollamaEndpoint));
        if (!isStructuredSuccess(report)) return report;
        // Absorbed the former `machine` tool. Attempted, not required: `doctor` must keep working
        // with no `freellama serve` running, because the Ollama half of the diagnostic is exactly
        // the half you need when things are broken. A failure degrades to a stated reason.
        // Prefer serve's profile when it is up (same portable OS discovery, plus the serve endpoint).
        // Never replace a native `machine` block with null — that hid chip/RAM when diagnosing
        // a downed serve, which is when doctor is most useful.
        try {
          report.structuredContent.machine = JSON.parse(await machine(serveEndpoint));
          delete report.structuredContent.machine_unavailable;
        } catch (error) {
          if (report.structuredContent.machine == null) {
            report.structuredContent.machine_unavailable =
              `freellama serve unreachable, so no machine profile: ${error instanceof Error ? error.message : String(error)}`;
          }
        }
        try {
          report.structuredContent.platform_health = JSON.parse(await health(serveEndpoint));
          report.structuredContent.readiness = await serveReadiness(serveEndpoint);
        } catch (error) {
          report.structuredContent.platform_health_unavailable =
            `freellama serve unreachable: ${error instanceof Error ? error.message : String(error)}`;
        }
        const full = { ...report.structuredContent };
        const running = (full.running as { models?: unknown[] } | undefined)?.models ?? [];
        const { status: overall, problems } = doctorStatus(full);
        const headline = `Ollama ${String((full.version as Record<string, unknown> | undefined)?.version ?? "unknown")}; ${running.length} resident model(s); ` +
          (problems.length > 0 ? problems.join("; ") : "freellama serve ready") + ".";
        if ((view ?? "summary") === "full") return structuredResult(full, { text: doctorFullText(full, headline) });
        const platform = full.platform_health as Fields | undefined;
        const compact = {
          status: overall,
          summary: headline,
          ...(problems.length > 0 ? { problems } : {}),
          endpoint: full.endpoint,
          ollama: { endpoint: full.endpoint, version: full.version, resident_model_count: running.length },
          platform_health: platform
            ? { status: platform.status, version: platform.version, readiness: full.readiness }
            : { unavailable: full.platform_health_unavailable },
          machine: full.machine,
          local_conservative_config_posture: full.local_conservative_config_posture,
          host_runtime_signals: full.host_runtime_signals,
          evidence: { runtime: "ollama_api_version_and_ps", configuration: full.ollama_env_config_source },
          next: "Use models{view:\"installed\"} to select a local model; view:\"full\" has Ollama configuration and admission.",
        };
        return structuredResult(compact);
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
