// doctor: Ollama health plus optional managed-serve views.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { doctor, health, machine, usage, status } from "../native.js";
import {
  endpointParam, errorResult, isStructuredSuccess, ollamaEndpointParam, parsedResult, structuredResult,
} from "../helpers.js";

const doctorResultSchema = z.object({
  summary: z.string().optional(), endpoint: z.string().optional(),
}).loose();

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
        const compact = {
          status: "ok",
          summary: `Ollama ${String((full.version as Record<string, unknown> | undefined)?.version ?? "unknown")}; ${running.length} resident model(s).`,
          endpoint: full.endpoint,
          ollama: { endpoint: full.endpoint, version: full.version, resident_model_count: running.length },
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
