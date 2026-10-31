// doctor: Ollama health plus optional managed-serve views.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { doctor, health, machine, usage, status } from "../native.js";
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
  return { status: problems.length > 0 ? "degraded" : "ok", problems };
}

export function registerDoctorTool(server: McpServer): void {
  server.registerTool(
    "doctor",
    {
      description: "Use when: checking whether Ollama/FreeLlama are healthy, reading live queues/limits (view:status) or token usage (view:usage). Do not use when: choosing a model (models). Returns: a short summary; pick a view for detail.",
      inputSchema: z.object({
        ollamaEndpoint: ollamaEndpointParam,
        serveEndpoint: endpointParam,
        view: z.enum(["summary", "full", "status", "usage"]).optional(),
        days: z.number().int().min(1).max(366).optional(),
      }).strict(),
      outputSchema: doctorResultSchema,
      annotations: { readOnlyHint: true },
    },
    async ({ ollamaEndpoint, serveEndpoint, view, days }) => {
      try {
        if (days !== undefined && view !== "usage") return errorResult(new Error("days is only valid with view: usage."));
        // Live views come from serve alone; they do not need the Ollama half of the diagnostic.
        if (view === "status") return parsedResult(await status(serveEndpoint));
        if (view === "usage") return parsedResult(await usage(serveEndpoint, days));
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
        // `ollama_config.categories` is the canonical categorized form (configuration and scheduler
        // settings included); the flat `ollama_env_config` duplicated it, so it is not sent.
        delete full.ollama_env_config;
        if ((view ?? "summary") === "full") return structuredResult(full);
        const running = (full.running as { models?: unknown[] } | undefined)?.models ?? [];
        const { status: overall, problems } = doctorStatus(full);
        const platform = full.platform_health as Fields | undefined;
        const compact = {
          status: overall,
          summary: `Ollama ${String((full.version as Record<string, unknown> | undefined)?.version ?? "unknown")}; ${running.length} resident model(s); ` +
            (problems.length > 0 ? problems.join("; ") : "freellama serve ready") + ".",
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
