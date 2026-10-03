#!/usr/bin/env node
/**
 * MCP server: thin wrappers over FreeLlama core (NAPI) plus Ollama lifecycle and
 * grounded research. Tool schemas are re-sent every request — keep them short.
 * Measured model guidance lives in docs/MODEL_SELECTION.md, not in these descriptions.
 *
 * doctor: Ollama directly. models library: ollama.com. run_task / installed and resident lists:
 * need serve (:11435). delegate_research: adapter subprocess + Ollama (or the serve proxy).
 */
import { type ChildProcess, execFile } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import {
  REPO_ROOT,
  RESEARCH_ADAPTERS,
  type ResearchAdapter,
  DEFAULT_RESEARCH_ADAPTER,
  DEFAULT_SERVE_ENDPOINT,
  DEFAULT_DELEGATE_MODEL,
  DEFAULT_DELEGATE_MAX_TURNS,
  DEFAULT_DELEGATE_TIMEOUT_SECONDS,
  DEFAULT_PULL_TIMEOUT_SECONDS,
  DEFAULT_TOKEN_CALIBRATION_DIR,
  assertAllowedWorkspace,
  delegateEnvironment,
} from "./config.js";
import * as native from "./native.js";
import { doctor, machine, health, status, usage, SERVER_VERSION } from "./native.js";
import { ensureServe, withServe, stopAutostartedServe } from "./serve.js";
import {
  ollamaFetch,
  ollamaPull,
  parseAdapterResult,
  endpointParam,
  ollamaEndpointParam,
  taskParam,
  systemPromptParam,
  messagesParam,
  localToolsParam,
  taskMessages,
  batchItemParam,
  canonicalTaskKind,
  objectiveParam,
  executionPreferenceParam,
  minPlacementEvidenceParam,
  minConfidenceParam,
  belowConfidence,
  requiredCapabilitiesParam,
  clipText,
  structuredResult,
  isStructuredSuccess,
  taskAnswerText,
  batchAnswerText,
  ANSWER_TEXT_MAX_CHARS,
  parsedResult,
  errorResult,
  summarizeEmbeddings,
  extractExistingWorkspacePath,
  withRequiredCapability,
  objectResultSchema,
  doctorResultSchema,
  sessionResultSchema,
  manageResultSchema,
  deleteResultSchema,
  modelsResultSchema,
  taskResultSchema,
  batchResultSchema,
  researchResultSchema,
  configuredExternalCost,
  costTelemetry,
} from "./helpers.js";
import { libraryTrialCandidate } from "./model-search.js";
import { enrichInstalledModels, libraryReference, modelKnowledge, OllamaLibraryClient, publicModelGuidance } from "./model-knowledge.js";
import { MODEL_EVIDENCE, assessDelegatedAnswer, researchErrorResult } from "./delegate.js";

import { registerContextTools } from "./context-tools.js";

const execFileAsync = promisify(execFile);

const createSession = withServe(native.createSession);
const deleteSession = withServe(native.deleteSession);
const killSession = withServe(native.killSession);
const listModels = withServe(native.listModels);
const ollamaLibrary = new OllamaLibraryClient();
const route = withServe(native.route);
const runTaskRequest = withServe(native.runTaskRequest);
const runTaskBatchRequest = withServe(native.runTaskBatchRequest);
const listTaskJobs = withServe(native.listTaskJobs);
const getTaskJob = withServe(native.getTaskJob);
const cancelTaskJob = withServe(native.cancelTaskJob);
const removeTaskJob = withServe(native.removeTaskJob);

type Page<T> = { items: T[]; returned: number; total: number; next_cursor: string | null };
const EXTERNAL_COST = configuredExternalCost();

/** Attach accounting after a successful managed response; the Rust layer remains provider-neutral. */
function withTaskTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!isStructuredSuccess(result)) return result;
  const payload = result.structuredContent as Record<string, unknown>;
  const metrics = payload.metrics as Record<string, unknown> | undefined;
  return structuredResult({
    ...payload,
    telemetry: costTelemetry({
      inputTokens: typeof metrics?.prompt_tokens === "number" ? metrics.prompt_tokens : null,
      outputTokens: typeof metrics?.output_tokens === "number" ? metrics.output_tokens : null,
      totalDurationNs: typeof metrics?.total_duration_ns === "number" ? metrics.total_duration_ns : null,
    }, EXTERNAL_COST),
  }, { text: taskAnswerText(payload) });
}

function withBatchTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!isStructuredSuccess(result)) return result;
  const payload = result.structuredContent as Record<string, unknown>;
  const rows = Array.isArray(payload.results) ? payload.results : [];
  let inputTokens = 0;
  let outputTokens = 0;
  let complete = 0;
  const results = rows.map((row) => {
    if (!row || typeof row !== "object") return row;
    const item = row as Record<string, unknown>;
    if (item.ok !== true || !item.response || typeof item.response !== "object") return item;
    const response = item.response as Record<string, unknown>;
    const metrics = response.metrics as Record<string, unknown> | undefined;
    const input = typeof metrics?.prompt_tokens === "number" ? metrics.prompt_tokens : null;
    const output = typeof metrics?.output_tokens === "number" ? metrics.output_tokens : null;
    if (input !== null && output !== null) {
      inputTokens += input;
      outputTokens += output;
      complete += 1;
    }
    return { ...item, response: { ...response, telemetry: costTelemetry({ inputTokens: input, outputTokens: output }, EXTERNAL_COST) } };
  });
  return structuredResult({
    ...payload,
    results,
    telemetry: complete === rows.filter((row) => (row as Record<string, unknown>)?.ok === true).length
      ? costTelemetry({ inputTokens, outputTokens }, EXTERNAL_COST)
      : { local: null, externalEquivalent: null, note: "Batch aggregate unavailable because one or more successful items omitted token counts." },
  }, { text: batchAnswerText(payload) });
}

/** Page a live list with an opaque cursor that refuses to continue after list drift. */
function pageLiveList<T>(items: T[], limit: number | undefined, cursor: string | undefined, identity: (item: T) => string): Page<T> {
  const pageSize = limit ?? 20;
  const fingerprint = createHash("sha256").update(items.map(identity).join("\n")).digest("base64url").slice(0, 16);
  let offset = 0;
  if (cursor) {
    try {
      const parsed = JSON.parse(Buffer.from(cursor, "base64url").toString("utf8")) as { offset?: unknown; fingerprint?: unknown };
      if (!Number.isInteger(parsed.offset) || (parsed.offset as number) < 0 || parsed.fingerprint !== fingerprint) {
        throw new Error("invalid or stale cursor");
      }
      offset = parsed.offset as number;
    } catch {
      throw new Error("`cursor` is invalid or the model list changed; restart without cursor.");
    }
  }
  const page = items.slice(offset, offset + pageSize);
  const nextOffset = offset + page.length;
  return {
    items: page,
    returned: page.length,
    total: items.length,
    next_cursor: nextOffset < items.length
      ? Buffer.from(JSON.stringify({ offset: nextOffset, fingerprint }), "utf8").toString("base64url")
      : null,
  };
}

// `delegate_research` spawns a python subprocess that can run for minutes. If this server goes
// away first — client disconnect, Ctrl-C, a supervisor restart — an untracked child keeps a local
// model pinned in VRAM with nothing left to return its answer to. Track every live child and take
// it down with the server.
const liveDelegates = new Set<ChildProcess>();

function killLiveDelegates(): void {
  for (const child of liveDelegates) {
    try {
      child.kill("SIGKILL");
    } catch {
      // Already gone; nothing to clean up.
    }
  }
  liveDelegates.clear();
}

// A client that disappears mid-response leaves the stdio transport writing to a closed pipe.
// Node surfaces that as an unhandled 'error' event on the socket, which killed the whole process
// with a stack trace on stderr (observed live). There is nothing to recover — the client is gone —
// but it should exit quietly through the normal path so the cleanup below still runs and a real
// error isn't buried in an EPIPE trace.
for (const stream of [process.stdout, process.stderr] as const) {
  stream.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EPIPE") process.exit(0);
    throw error;
  });
}

process.on("exit", () => {
  killLiveDelegates();
  stopAutostartedServe();
});
// A client that closes stdin without a signal (most MCP hosts on shutdown) left this process
// alive until every running delegate finished, holding a model loaded for nobody.
process.stdin.on("close", () => {
  killLiveDelegates();
  stopAutostartedServe();
  process.exit(0);
});

type ToolExtra = {
  signal: AbortSignal;
  _meta?: { progressToken?: string | number };
  sendNotification: (notification: {
    method: "notifications/progress";
    params: { progressToken: string | number; progress: number; total?: number; message?: string };
  }) => Promise<void>;
};

/** MCP progress for a pull: clients that sent a progressToken see bytes instead of a silent wait. */
function pullProgressReporter(extra: ToolExtra): ((event: Record<string, unknown>) => void) | undefined {
  const token = extra._meta?.progressToken;
  if (token === undefined) return undefined;
  let lastSent = 0;
  let step = 0;
  return (event) => {
    const now = Date.now();
    if (now - lastSent < 500 && event.status !== "success") return;
    lastSent = now;
    step += 1;
    const completed = typeof event.completed === "number" ? event.completed : undefined;
    const total = typeof event.total === "number" ? event.total : undefined;
    void extra.sendNotification({
      method: "notifications/progress",
      params: {
        progressToken: token,
        progress: completed ?? step,
        ...(completed !== undefined && total !== undefined ? { total } : {}),
        message: String(event.status ?? "pulling"),
      },
    }).catch(() => undefined);
  };
}
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
  process.on(signal, () => {
    killLiveDelegates();
    process.exit(0);
  });
}


const INSTRUCTIONS = `models{view:"installed"}; models{view:"resident"} for residency.
run_task preview never executes; code_review aliases coding. Preview routing only; requiredCapabilities:["tools"] previews tools; omit preview and supply the payload. Caller executes tools.
quality needs policy/model; medium needs policy+benchmark; observed needs /api/ps proof.
Scoped run_task needs scopeId+scopeRevision. Scopes store history; sessions affinity, not messages/KV. defer:true: job.id→task_jobs.jobId unchanged. Restart loses scopes/sessions/jobs.
Check isError first; prefer structuredContent, else content[].text. page.next_cursor→cursor unchanged.
caller owns task decomposition/prompts/format/verification; findings are candidates, not accepted defects. operator owns endpoints, exact --cpu-model assignments/lifecycle. Ollama plus the OS/driver run physical CPU/GPU.
ask approval for one exact tag and reported size before ollama_manage pull; search or recommendation is never download permission. Stop/delete need exact-tag approval.
serve :11435; default-endpoint autostart optional. Docs: freellama://docs/index.`;

const server = new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS },
);

// Documentation is bundled at build time from the repository docs/ directory. Resources keep it
// out of the always-present tool instruction budget while giving MCP clients an on-demand,
// package-local operating manual after npm installation.
const PACKAGED_DOCS_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "docs");
if (!existsSync(PACKAGED_DOCS_DIR)) {
  throw new Error(`FreeLlama MCP documentation is missing at ${PACKAGED_DOCS_DIR}; run the package build.`);
}
const packagedDocs = readdirSync(PACKAGED_DOCS_DIR)
  .filter((name) => name.endsWith(".md"))
  .sort();
if (!packagedDocs.includes("INDEX.md")) {
  throw new Error(`FreeLlama MCP documentation index is missing at ${PACKAGED_DOCS_DIR}/INDEX.md; run the package build.`);
}
for (const name of packagedDocs) {
  const uri = `freellama://docs/${name === "INDEX.md" ? "index" : name.replace(/\.md$/, "")}`;
  server.registerResource(
    `freellama-docs-${name.toLowerCase().replace(/\.md$/, "")}`,
    uri,
    {
      mimeType: "text/markdown",
      description: name === "INDEX.md"
        ? "Index of packaged FreeLlama operator and agent documentation. Read this first, then fetch one relevant document."
        : `Packaged FreeLlama documentation: ${name}.`,
    },
    async () => ({
      contents: [{ uri, mimeType: "text/markdown", text: await readFile(path.join(PACKAGED_DOCS_DIR, name), "utf8") }],
    }),
  );
}

server.registerTool(
  "doctor",
  {
    description: "Use when: diagnosis/status/usage. Do not use when: choosing models. Returns: summary; opt into detail.",
    inputSchema: z.object({
      endpoint: ollamaEndpointParam,
      serveEndpoint: endpointParam,
      view: z.enum(["summary", "scheduler", "config", "full", "status", "usage"]).optional(),
      days: z.number().int().min(1).max(366).optional(),
    }).strict(),
    outputSchema: doctorResultSchema,
    annotations: { readOnlyHint: true },
  },
  async ({ endpoint, serveEndpoint, view, days }) => {
    try {
      if (days !== undefined && view !== "usage") return errorResult(new Error("days is only valid with view: usage."));
      // Live views come from serve alone; they do not need the Ollama half of the diagnostic.
      if (view === "status") return parsedResult(await status(serveEndpoint));
      if (view === "usage") return parsedResult(await usage(serveEndpoint, days));
      const report = parsedResult(await doctor(endpoint));
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
      // `ollama_config.categories.memory_scheduler` is the canonical categorized form. The former
      // flat `ollama_env_config` duplicated it (~25% of a live full doctor result), so keep it
      // only in the internal derivation below and do not send it in verbose results.
      const flatConfig = full.ollama_env_config as Record<string, unknown> | undefined;
      delete full.ollama_env_config;
      if (view === "config") {
        return structuredResult({
          status: "ok",
          summary: "Categorized Ollama configuration; values are source-qualified, not endpoint/PID proof.",
          endpoint: full.endpoint,
          ollama_config: full.ollama_config,
          ollama_env_config_source: full.ollama_env_config_source,
          ollama_env_config_warning: full.ollama_env_config_warning,
          host_runtime_signals: full.host_runtime_signals,
        });
      }
      if ((view ?? "summary") === "full") return structuredResult(full);
      const running = (full.running as { models?: unknown[] } | undefined)?.models ?? [];
      const platformHealth = full.platform_health as Record<string, unknown> | undefined;
      const compact = {
        status: "ok",
        summary: `Ollama ${String((full.version as Record<string, unknown> | undefined)?.version ?? "unknown")}; ${running.length} resident model(s).`,
        endpoint: full.endpoint,
        ollama: { endpoint: full.endpoint, version: full.version, resident_model_count: running.length },
        machine: full.machine,
        local_conservative_config_posture: full.local_conservative_config_posture,
        host_runtime_signals: full.host_runtime_signals,
        ...(view === "scheduler" ? {
          scheduler: {
            admission: platformHealth?.admission ?? null,
            ollama_num_parallel: flatConfig?.OLLAMA_NUM_PARALLEL,
            ollama_max_queue: flatConfig?.OLLAMA_MAX_QUEUE,
            proof_level: "configured_and_snapshot_only; measure concurrent execution before claiming throughput",
          },
        } : {}),
        evidence: { runtime: "ollama_api_version_and_ps", configuration: full.ollama_env_config_source },
        next: view === "scheduler" ? "Use run_task preview for a per-task advisory receipt." : "Use models{view:\"installed\"} to select a local model.",
      };
      return structuredResult(compact);
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "session",
  {
    description:
      "Use when: affinity/session kill. Do not use when: job cancel/history/unload. Returns: receipt.",
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

server.registerTool(
  "models",
  {
    description:
      "Use when: inventory/library. Do not use when: execution/mutation. " +
      "Returns: models; library finds families, then one family's tags.",
    inputSchema: z.object({
      view: z
        .enum(["installed", "resident", "detail", "raw", "library"])
        .optional()
        ,
      model: z.string().min(1).optional(),
      includeVerbose: z
        .boolean()
        .optional()
        ,
      includeLibrary: z.boolean().optional(),
      includeReadme: z.boolean().optional(),
      query: z.string().min(1).optional(),
      capabilities: z
        .array(z.enum(["vision", "tools", "thinking", "embedding", "cloud"]))
        .min(1)
        .max(5)
        .optional()
        ,
      order: z
        .enum(["popular", "newest"])
        .optional()
        ,
      limit: z.number().int().positive().max(50).optional(),
      cursor: z.string().min(1).optional(),
      endpoint: endpointParam,
      ollamaEndpoint: ollamaEndpointParam,
    }).strict(),
    outputSchema: modelsResultSchema,
    annotations: { readOnlyHint: true },
  },
  async ({ view, model, includeVerbose, includeLibrary, includeReadme, query, capabilities, order, limit, cursor, endpoint, ollamaEndpoint }) => {
    try {
      const selectedView = view ?? "installed";
      if (includeLibrary !== undefined && selectedView !== "installed" && selectedView !== "detail") {
        return errorResult(new Error('`includeLibrary` is valid only for views "installed" and "detail".'));
      }
      if (includeReadme !== undefined && !(selectedView === "library" && model || selectedView === "detail" && includeLibrary === true)) {
        return errorResult(new Error('`includeReadme` needs library step 2, or view "detail" with `includeLibrary:true`.'));
      }
      if (selectedView !== "library" && [query, capabilities, order].some((value) => value !== undefined)) {
        return errorResult(new Error(`view "${selectedView}" does not accept library search fields.`));
      }
      if (selectedView !== "library" && selectedView !== "raw" && [limit, cursor].some((value) => value !== undefined)) {
        return errorResult(new Error(`view "${selectedView}" accepts neither pagination nor library search fields.`));
      }
      if (selectedView !== "detail" && includeVerbose !== undefined) {
        return errorResult(new Error('`includeVerbose` is valid only for view "detail".'));
      }
      if (selectedView !== "detail" && selectedView !== "library" && model !== undefined) {
        return errorResult(new Error('`model` is valid only for views "detail" and "library".'));
      }
      if (
        selectedView === "library" &&
        model &&
        [query, capabilities, order].some((value) => value !== undefined)
      ) {
        return errorResult(
          new Error('Library step 2 accepts only `model` plus endpoint overrides; omit step-1 search fields.'),
        );
      }

      switch (selectedView) {
        case "raw": {
          const raw = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as Record<string, unknown>;
          const models = Array.isArray(raw.models) ? raw.models : [];
          const page = pageLiveList(models, limit, cursor, (model) => String((model as Record<string, unknown>).name ?? JSON.stringify(model)));
          return structuredResult({ ...raw, models: page.items, page: { returned: page.returned, total: page.total, next_cursor: page.next_cursor } });
        }

        case "resident": {
          const managed = parsedResult(await listModels(endpoint));
          if (!isStructuredSuccess(managed)) return managed;
          const data = managed.structuredContent as {
            models?: Array<Record<string, unknown>>;
          };
          // Ollama's own docs say to check the GPU/CPU split, but /api/ps exposes only the raw
          // `size`/`size_vram` bytes it is derived from. The CLI computes it; the API doesn't.
          const models = (data.models ?? []).filter((entry) => entry.resident === true).map((entry) => {
            const size = typeof entry.resident_size === "number" ? entry.resident_size : null;
            const vram = typeof entry.resident_vram === "number" ? entry.resident_vram : null;
            if (size === null || vram === null || size === 0) return entry;
            const gpuPercent = Math.round((vram / size) * 100);
            const execution = entry.execution as {
              placement?: string;
              backend?: string;
              observation?: { processor?: string; status?: string; source?: string };
            } | undefined;
            const assignedCpu = execution?.placement === "cpu";
            const observedProcessor = execution?.observation?.processor;
            const processor =
              observedProcessor === "cpu" || observedProcessor === "gpu" || observedProcessor === "mixed"
                ? observedProcessor
                : gpuPercent >= 100
                  ? "gpu"
                  : gpuPercent <= 0
                    ? "cpu"
                    : "mixed";
            const mismatch = execution?.observation?.status === "mismatch";
            return {
              ...entry,
              placement: {
                gpu_percent: gpuPercent,
                assigned: assignedCpu,
                processor:
                  processor === "gpu"
                    ? "100% GPU"
                    : processor === "cpu"
                      ? "100% CPU"
                      : `${gpuPercent}% GPU / ${100 - gpuPercent}% CPU`,
                ...(mismatch
                  ? {
                      warning:
                        `Configured ${execution?.placement ?? "unknown"} backend disagrees with Ollama /api/ps: ` +
                        `${processor} was physically observed. This sample is excluded from adaptive routing feedback.`,
                    }
                  : processor === "mixed"
                  ? {
                      warning:
                        "Partially offloaded to CPU — expect a large slowdown. Free VRAM (`ollama_manage` action \"stop\") or lower the context length.",
                    }
                  : {}),
              },
            };
          });
          return structuredResult({ ...data, models });
        }

        case "detail": {
          if (!model) {
            return errorResult(new Error('view "detail" needs `model` set to an installed tag.'));
          }
          const data = (await ollamaFetch(ollamaEndpoint, "/api/show", {
            method: "POST",
            body: { model },
          })) as Record<string, unknown>;
          let local = data;
          if (includeLibrary && !data.remote_host && !data.remote_model && !/-cloud$/i.test(model)) {
            // /show does not guarantee a manifest digest. Use the exact /tags entry, never a name guess.
            try {
              const inventory = await ollamaFetch(ollamaEndpoint, "/api/tags") as { models?: Array<Record<string, unknown>> };
              const canonical = libraryReference(model)?.tag;
              const entry = canonical && inventory.models?.find((entry) => libraryReference(String(entry.name ?? entry.model))?.tag === canonical);
              if (entry) local = { ...entry, ...data, digest: entry.digest };
            } catch { /* Without a digest, enrichment reports unverified and withholds public guidance. */ }
          }
          const library = includeLibrary && !local.remote_host && !local.remote_model && !/-cloud$/i.test(model)
            ? await ollamaLibrary.lookup(model) : undefined;
          const { license, modelfile, ...rest } = data;
          // The real ceiling hides under a per-architecture key (`qwen3_5.context_length`,
          // `llama.context_length`, ...), so it cannot be read by a fixed path.
          const modelInfo = (rest.model_info ?? {}) as Record<string, unknown>;
          const contextEntry = Object.entries(modelInfo).find(
            ([key, value]) => key.endsWith(".context_length") && typeof value === "number",
          );
          return structuredResult({
            ...rest,
            max_context_length: (contextEntry?.[1] as number) ?? null,
            knowledge: modelKnowledge(model, local, library, includeReadme),
            ...(includeVerbose ? { license, modelfile } : {}),
          });
        }

        case "library":
          return await libraryLookup({ model, query, capabilities, order, limit, cursor, includeReadme, endpoint, ollamaEndpoint });

        default: {
          const result = parsedResult(await listModels(endpoint));
          if (!isStructuredSuccess(result)) return result;
          const data = result.structuredContent as { models?: Array<Record<string, unknown>> };
          return structuredResult({ ...data, models: await enrichInstalledModels(data.models ?? [], ollamaLibrary, includeLibrary ?? false,
            async (upstream) => (await ollamaFetch(upstream ?? ollamaEndpoint, "/api/tags") as { models?: Array<Record<string, unknown>> }).models ?? []) });
        }
      }
    } catch (error) {
      return errorResult(error);
    }
  },
);

// `route` was folded into `run_task { preview: true }` — same NAPI `route()` call, no second
// schema. `search_models` was folded into `models { view: "library" }` — same two-step lookup.
async function libraryLookup({
  model,
  query,
  capabilities,
  order,
  limit,
  cursor,
  includeReadme,
  endpoint,
  ollamaEndpoint,
}: {
  model?: string;
  query?: string;
  capabilities?: Array<"vision" | "tools" | "thinking" | "embedding" | "cloud">;
  order?: "popular" | "newest";
  limit?: number;
  cursor?: string;
  includeReadme?: boolean;
  endpoint?: string;
  ollamaEndpoint?: string;
}) {
    const localState = async () => {
      let installed = new Set<string>();
      let memoryBytes: number | null = null;
      try {
        const tags = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as { models?: Array<{ name?: string }> };
        installed = new Set((tags.models ?? []).map((m) => m.name ?? ""));
      } catch {
        /* Ollama unreachable */
      }
      try {
        const profile = JSON.parse(await machine(endpoint));
        memoryBytes = profile.memory_bytes ?? profile.unified_memory_bytes ?? null;
      } catch {
        /* serve unreachable */
      }
      return { installed, memoryBytes };
    };

    try {
      if (model) {
        const ref = libraryReference(model);
        if (!ref) throw new Error("Use an exact Ollama family or namespace/model name, not a URL or path.");
        const { family } = ref;
        const library = await ollamaLibrary.lookup(model);
        if (library.status !== "available") return structuredResult({ family, sourcePage: ref.url, tags: [],
          tagsUnavailable: library.message, library, recommendation: null });
        const tags = library.tags!;
        const { installed, memoryBytes } = await localState();
        const budget = memoryBytes ? memoryBytes * 0.6 : null;
        const annotated = tags.map((t) => ({
          ...t,
          installed: installed.has(t.tag),
          fitsInMemory: budget && t.sizeBytes ? t.sizeBytes <= budget : null,
          fitScope: "host_memory_budget_only",
        }));
        const runnable = annotated.filter((t) => t.fitsInMemory === true && !t.cloud);
        const isEmbeddingFamily = library.card!.capabilities.includes("embedding");
        const sized = runnable.filter((t) => t.sizeBytes);
        const best = budget
          ? isEmbeddingFamily
            ? sized.sort((a, b) => (a.sizeBytes ?? 0) - (b.sizeBytes ?? 0))[0]
            : sized.sort((a, b) => (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0))[0]
          : undefined;
        const tagPage = pageLiveList(annotated, limit, cursor, (tag) => tag.tag);
        return structuredResult({
          family,
          sourcePage: ref.url,
          library: { status: library.status, sourceUrl: library.sourceUrl, tagsSourceUrl: library.tagsSourceUrl,
            fetchedAt: library.fetchedAt, expiresAt: library.expiresAt, cached: library.cached,
            ...publicModelGuidance(library, undefined, includeReadme) },
          ...(tags.length === 0
            ? {
                tagsUnavailable:
                  `No pullable tags found for "${family}". Either it is cloud-only (no local ` +
                  "download), the family name is wrong, or ollama.com changed its markup. Open " +
                  "the page above to check before concluding the model does not exist.",
              }
            : {}),
          machineMemoryBytes: memoryBytes,
          fitBudgetBytes: budget,
          tags: tagPage.items,
          page: { returned: tagPage.returned, total: tagPage.total, next_cursor: tagPage.next_cursor },
          recommendationUnavailable: budget
            ? undefined
            : "No machine profile (freellama serve unreachable), so memory fit could not be checked and no tag is recommended. Start serve, or read the sizes yourself.",
          recommendation: best ? libraryTrialCandidate(best.tag, isEmbeddingFamily) : null,
        });
      }

      const params = new URLSearchParams();
      for (const c of capabilities ?? []) params.append("c", c);
      if (query) params.set("q", query);
      if (order === "newest") params.set("o", "newest");
      const search = await ollamaLibrary.search(params);
      const url = search.url;
      const parsed = search.models.slice(0, limit ?? 10);
      const { installed } = await localState();
      const installedFamilies = new Set([...installed].map((n) => n.split(":")[0]));
      return structuredResult({
        query: url,
        fetchedAt: search.fetchedAt,
        ...(parsed.length === 0 ? { searchUnavailable: "No result cards were recognized. The query may have no matches or Ollama's markup may have changed; inspect the source URL." } : {}),
        order: order ?? "popular",
        count: parsed.length,
        nextStep: 'Not pullable yet. Call again with view:"library", model:"<name>" to get tags, sizes and memory fit.',
        models: parsed.map((m) => ({ ...m, installed: installedFamilies.has(m.name) })),
      });
    } catch (error) {
      return errorResult(error);
    }
}

registerContextTools(server);

server.registerTool(
  "run_task",
  {
    description:
      "Use when: supplied chat/tools/embeddings. Do not use when: files. Returns: response/receipts.",
    inputSchema: z.object({
      endpoint: endpointParam,
      task: taskParam.removeDefault().optional().describe("caller owns prompts and output format."),
      objective: objectiveParam,
      model: z.string().min(1).optional(),
      sessionId: z.string().uuid().optional(),
      scopeId: z.string().uuid().optional(),
      scopeRevision: z.number().int().nonnegative().optional(),
      contextTokens: z.number().int().positive().optional().describe("Total input + output window (num_ctx)."),
      executionPreference: executionPreferenceParam,
      minPlacementEvidence: minPlacementEvidenceParam,
      requiredCapabilities: requiredCapabilitiesParam,
      prompt: z.string().min(1).optional(),
      systemPrompt: systemPromptParam,
      images: z
        .array(z.string().min(1))
        .min(1)
        .optional()
        .describe("base64, no data-URI prefix; prompt mode only; requires an explicit tested vision model"),
      messages: messagesParam.describe(
          "Caller system prompts; no injected task instructions. Overrides prompt; extra fields preserved.",
        ),
      input: z
        .union([z.string().min(1), z.array(z.string().min(1)).min(1)])
        .optional()
        ,
      tools: localToolsParam,
      keepAlive: z.string().min(1).optional().describe('"0" unloads now, "-1" pins; omitted = adaptive finite TTL'),
      format: z
        .union([z.literal("json"), z.record(z.unknown())])
        .optional()
        ,
      think: z
        .union([z.boolean(), z.enum(["low", "medium", "high"])])
        .optional(),
      options: z
        .record(z.unknown())
        .optional()
        .describe("num_ctx/contextTokens and num_gpu/placement are routing-owned; num_predict caps output."),
      logprobs: z.boolean().optional(),
      topLogprobs: z.number().int().nonnegative().optional(),
      minConfidence: minConfidenceParam,
      priority: z.enum(["interactive", "normal", "background"]).optional(),
      maxWaitSeconds: z.number().int().positive().optional(),
      timeoutSeconds: z.number().int().positive().optional(),
      defer: z.boolean().optional(),
      returnEmbeddings: z.boolean().optional(),
      preview: z
        .boolean()
        .optional()
        .describe(
          "true = routing fields only; rejects payloads and runtime controls; never executes",
        ),
    }).strict(),
    outputSchema: taskResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({
    endpoint,
    task,
    objective,
    model,
    sessionId,
    scopeId,
    scopeRevision,
    contextTokens,
    executionPreference,
    minPlacementEvidence,
    requiredCapabilities,
    prompt,
    systemPrompt,
    images,
    messages,
    input,
    tools,
    keepAlive,
    format,
    think,
    options,
    logprobs,
    topLogprobs,
    minConfidence,
    priority,
    maxWaitSeconds,
    timeoutSeconds,
    defer,
    returnEmbeddings,
    preview,
  }) => {
    try {
      const canonicalTask = canonicalTaskKind(task ?? "completion");
      if ((scopeId === undefined) !== (scopeRevision === undefined)) {
        return errorResult(new Error("scopeId and scopeRevision must be supplied together."));
      }
      if (preview) {
        const executionOnlyFields = ([
          ["scopeId", scopeId],
          ["scopeRevision", scopeRevision],
          ["prompt", prompt],
          ["systemPrompt", systemPrompt],
          ["images", images],
          ["messages", messages],
          ["input", input],
          ["tools", tools],
          ["keepAlive", keepAlive],
          ["format", format],
          ["think", think],
          ["options", options],
          ["logprobs", logprobs],
          ["topLogprobs", topLogprobs],
          ["returnEmbeddings", returnEmbeddings],
          ["maxWaitSeconds", maxWaitSeconds],
          ["priority", priority],
          ["timeoutSeconds", timeoutSeconds],
          ["defer", defer],
        ] satisfies Array<[string, unknown]>)
          .filter(([, value]) => value !== undefined)
          .map(([name]) => `\`${name}\``);
        if (executionOnlyFields.length > 0) {
          return errorResult(
            new Error(
              "`preview:true` accepts routing fields only; remove execution-only fields " +
                `${executionOnlyFields.join(", ")}. Use \`requiredCapabilities:[\"tools\"]\` ` +
                "to preview tool capability, then make a separate execution call with the payload.",
            ),
          );
        }
      }
      const effectiveRequiredCapabilities =
        tools === undefined
          ? requiredCapabilities
          : withRequiredCapability(requiredCapabilities, "tools");
      if (topLogprobs !== undefined && logprobs !== true) {
        return errorResult(new Error("`topLogprobs` requires `logprobs:true`."));
      }
      if (!preview && task === "embedding") {
        if (input === undefined) return errorResult(new Error('task "embedding" requires `input`.'));
        if (
          [prompt, systemPrompt, images, messages, tools, format, think, logprobs, topLogprobs].some(
            (value) => value !== undefined,
          )
        ) {
          return errorResult(
            new Error('task "embedding" accepts `input`, `options`, and routing fields; chat controls are not valid.'),
          );
        }
      }
      if (!preview && task !== "embedding") {
        if (input !== undefined || returnEmbeddings !== undefined) {
          return errorResult(new Error('`input` and `returnEmbeddings` are valid only for task "embedding".'));
        }
        if (prompt === undefined && systemPrompt === undefined && (messages === undefined || messages.length === 0)) {
          return errorResult(new Error(`task "${task}" requires \`prompt\`, \`systemPrompt\`, or \`messages\`.`));
        }
        if (images !== undefined && (messages !== undefined || prompt === undefined)) {
          return errorResult(
            new Error("Top-level `images` is valid only with `prompt`; put images inside messages instead."),
          );
        }
        if (images !== undefined && model === undefined) {
          return errorResult(new Error("Images require an explicit tested vision-capable `model`."));
        }
      }
      if (preview) {
        const result = parsedResult(
          await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
        );
        if (isStructuredSuccess(result)) {
          const refusal = belowConfidence(result.structuredContent, minConfidence);
          if (refusal) return refusal;
        }
        return result;
      }
      // Gating after the fact would be useless here: by the time `run_task` returns, the tokens
      // are spent. So when the caller sets a floor, preview the decision with a `route` call
      // first — free, no generation — and refuse before anything runs. Only costs the extra round
      // trip when the option is actually used.
      if (minConfidence && scopeId === undefined) {
        const decision = parsedResult(
          // minConfidence is forwarded so the CORE gate refuses, with its actionable message naming
        // the two commands that raise the grade. The belowConfidence() check below stays only as a
          // fallback for servers older than the core gate.
        await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
        );
        if (!isStructuredSuccess(decision)) return decision;
        const refusal = belowConfidence(decision.structuredContent, minConfidence);
        if (refusal) return refusal;
      }
      const result = parsedResult(
        await runTaskRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
          task: scopeId !== undefined && task === undefined ? undefined : canonicalTask,
          objective: objective ?? (scopeId === undefined ? "balanced" : undefined),
          model,
          session_id: sessionId,
          scope_id: scopeId,
          scope_revision: scopeRevision,
          context_tokens: contextTokens,
          required_capabilities: effectiveRequiredCapabilities,
          prompt,
          images,
          messages: taskMessages({ systemPrompt, messages, prompt, images }),
          input,
          tools,
          keep_alive: keepAlive,
          min_confidence: minConfidence,
          priority: priority ?? "normal",
          max_wait_seconds: maxWaitSeconds,
          timeout_seconds: timeoutSeconds,
          defer: defer ?? false,
          execution_preference: executionPreference,
          min_placement_evidence: minPlacementEvidence,
          request_options: {
            format,
            think,
            options,
            logprobs,
            top_logprobs: topLogprobs,
          },
        }),
      );
      if (isStructuredSuccess(result) && result.structuredContent.deferred === true) return result;
      if (!returnEmbeddings && isStructuredSuccess(result)) {
        const trimmed = summarizeEmbeddings(result.structuredContent);
        if (trimmed) return withTaskTelemetry(structuredResult(trimmed));
      }
      return withTaskTelemetry(result);
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "task_jobs",
  {
    description:
      "Use when: task jobs. Do not use when: new work/session kill. " +
      "Returns: status/result; remove cancels first.",
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

server.registerTool(
  "run_task_batch",
  {
    description:
      "Use when: independent:true work. Do not use when: dependencies. " +
      "Returns: ordered receipts; maxParallelism caps dispatch.",
    inputSchema: z.object({
      tasks: z.array(batchItemParam).min(1).max(64),
      maxParallelism: z.number().int().positive().max(64).optional(),
      endpoint: endpointParam,
    }).strict(),
    outputSchema: batchResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ tasks, maxParallelism, endpoint }) => {
    try {
      for (const item of tasks) {
        const task = item.task as Record<string, unknown>;
        if ((task.scopeId === undefined) !== (task.scopeRevision === undefined)) {
          return errorResult(new Error(`batch item ${item.id}: scopeId and scopeRevision must be supplied together.`));
        }
        if (task.task === "embedding") {
          if (task.input === undefined || task.prompt !== undefined || task.systemPrompt !== undefined || task.messages !== undefined || task.tools !== undefined) {
            return errorResult(new Error(`batch item ${item.id}: embedding requires input and accepts no chat payload.`));
          }
        } else if (task.input !== undefined || (task.prompt === undefined && task.systemPrompt === undefined && task.messages === undefined)) {
          return errorResult(new Error(`batch item ${item.id}: chat work requires prompt, systemPrompt, or messages and accepts no input.`));
        }
        if (task.images !== undefined && (task.messages !== undefined || task.prompt === undefined)) {
          return errorResult(new Error(`batch item ${item.id}: top-level images requires prompt without messages; put images inside messages instead.`));
        }
        if (task.images !== undefined && task.model === undefined) {
          return errorResult(new Error(`batch item ${item.id}: images require an explicit tested vision model.`));
        }
      }
      return withBatchTelemetry(parsedResult(await runTaskBatchRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
        tasks: tasks.map((item) => {
          const task = item.task;
          return ({
          id: item.id,
          independent: item.independent,
          task: {
            task: task.task === "code_review" ? "coding" : (task.task ?? (task.scopeId === undefined ? "completion" : undefined)),
            objective: task.objective ?? (task.scopeId === undefined ? "balanced" : undefined),
            model: task.model,
            session_id: task.sessionId,
            scope_id: task.scopeId,
            scope_revision: task.scopeRevision,
            context_tokens: (task as Record<string, unknown>).contextTokens,
            execution_preference: task.executionPreference,
            min_placement_evidence: task.minPlacementEvidence,
            required_capabilities: task.requiredCapabilities,
            priority: task.priority ?? "normal",
            max_wait_seconds: task.maxWaitSeconds,
            timeout_seconds: task.timeoutSeconds,
            prompt: task.prompt,
            images: task.images,
            messages: taskMessages(task),
            input: task.input,
            tools: (task as Record<string, unknown>).tools,
            keep_alive: (task as Record<string, unknown>).keepAlive,
            min_confidence: (task as Record<string, unknown>).minConfidence,
            request_options: {
              format: task.format,
              think: task.think,
              options: task.options,
              logprobs: task.logprobs,
              top_logprobs: task.topLogprobs,
            },
          },
        });
        }),
        max_parallelism: maxParallelism,
      })));
    } catch (error) {
      return errorResult(error);
    }
  },
);

// Ollama lifecycle: talk to Ollama directly (no serve). `models` covers list/ps/show.
// `ollama_delete` stays its own tool so pull/stop are not marked destructive.

server.registerTool(
  "ollama_manage",
  {
    description:
      "Use when: approved pull/stop. Do not use when: delete/discovery. " +
      "Returns: receipt; timeoutSeconds is pull-only.",
    inputSchema: z.object({
      action: z.enum(["pull", "stop"]),
      model: z.string().min(1),
      ollamaEndpoint: ollamaEndpointParam,
      timeoutSeconds: z
        .number()
        .int()
        .positive()
        .optional()
        ,
    }).strict(),
    outputSchema: manageResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ action, model, ollamaEndpoint, timeoutSeconds }, extra) => {
    try {
      if (action === "stop" && timeoutSeconds !== undefined) {
        return errorResult(new Error('`timeoutSeconds` is valid only for action "pull".'));
      }
      const data =
        action === "pull"
          ? await ollamaPull(ollamaEndpoint, model, timeoutSeconds, {
              signal: extra.signal,
              onProgress: pullProgressReporter(extra),
            })
          : await ollamaFetch(ollamaEndpoint, "/api/generate", {
              method: "POST",
              body: { model, keep_alive: 0 },
              signal: extra.signal,
            });
      const payload = data as Record<string, unknown>;
      // Pull failures arrive as an {"error": ...} event on an HTTP 200 stream, so the fetch
      // above succeeds; report them as errors or a caller gating on isError believes the model
      // is installed.
      if (typeof payload.error === "string" && payload.error) {
        return errorResult(new Error(`${action} ${model} failed: ${payload.error}`));
      }
      return structuredResult(payload);
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "ollama_delete",
  {
    description:
      "DESTRUCTIVE AND IRREVERSIBLE. Use when: human names exact tag. Do not use when: inferred cleanup. Returns: deleted tag.",
    inputSchema: z.object({
      model: z.string().min(1),
      ollamaEndpoint: ollamaEndpointParam,
    }).strict(),
    outputSchema: deleteResultSchema,
    annotations: { destructiveHint: true },
  },
  async ({ model, ollamaEndpoint }) => {
    try {
      await ollamaFetch(ollamaEndpoint, "/api/delete", {
        method: "DELETE",
        body: { model },
      });
      return structuredResult({ deleted: model });
    } catch (error) {
      return errorResult(error);
    }
  },
);

/**
 * Verdicts in `assessDelegatedAnswer` are per-model, from on-disk evidence — never a global
 * base rate. Grades live in benchmark/evidence/model-evidence.json.
 */
/** With no explicit or configured model, ask the router for an installed coding model. */
async function routedResearchModel(
  endpoint: string | undefined,
  executionPreference: string | undefined,
  minPlacementEvidence: string | undefined,
): Promise<string> {
  const decision = JSON.parse(
    await route(endpoint, "coding", "balanced", undefined, undefined, undefined, [], undefined, executionPreference, minPlacementEvidence),
  ) as { selected_model?: unknown };
  if (typeof decision.selected_model !== "string" || !decision.selected_model) {
    throw new Error(
      "No research model: pass `model`, set FREELLAMA_MCP_DEFAULT_MODEL, or install a coding model " +
        "(models{view:'installed'} shows what routing can use).",
    );
  }
  return decision.selected_model;
}

server.registerTool(
  "delegate_research",
  {
    description:
      "Use when: workspace lookup. Do not use when: mutation/external facts. " +
      "Returns: citations/answer; discard verification.recommendation=escalate, otherwise verify citations.",
    inputSchema: z.object({
      question: z.string().min(1),
      workspacePath: z
        .string()
        .min(1)
        ,
      adapter: z
        .enum(["bash", "octocode"])
        .optional()
        ,
      model: z
        .string()
        .min(1)
        .optional()
        ,
      endpoint: endpointParam,
      executionPreference: executionPreferenceParam,
      minPlacementEvidence: minPlacementEvidenceParam,
      legacyText: z.boolean().optional(),
      agent: z
        .object({
          maxTurns: z.number().int().positive().optional(),
          contextTokens: z.number().int().positive().optional(),
          outputTokens: z.number().int().positive().optional(),
          temperature: z.number().nonnegative().optional(),
          seed: z.number().int().nonnegative().optional(),
          think: z.boolean().optional(),
          keepAlive: z.string().min(1).optional(),
          requestTimeoutSeconds: z.number().positive().optional(),
          toolTimeoutSeconds: z.number().positive().optional(),
        })
        .strict()
        .optional()
        ,
    }).strict(),
    outputSchema: researchResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ question, workspacePath, adapter, model, endpoint, executionPreference, minPlacementEvidence, legacyText, agent }, extra) => {
    const chosenAdapter: ResearchAdapter = adapter ?? DEFAULT_RESEARCH_ADAPTER;
    let resolvedWorkspace: string;
    let chosenModel: string;
    try {
      resolvedWorkspace = await assertAllowedWorkspace(workspacePath);
      chosenModel = model ?? DEFAULT_DELEGATE_MODEL ?? await routedResearchModel(endpoint, executionPreference, minPlacementEvidence);
    } catch (error) {
      return researchErrorResult(error);
    }
    // Pre-flight, not post-hoc: a model this repo measured at 0-38% will not become right by
    // running it, so refuse before spending a model load and 10-40s of wall time on it.
    const known = MODEL_EVIDENCE[chosenModel];
    if (known?.grade === "unusable") {
      return structuredResult({
        adapter: chosenAdapter,
        verification: assessDelegatedAnswer(question, 0, chosenModel),
        answer: "",
        toolCallCount: 0,
        usage: { inputTokens: null, outputTokens: null },
        telemetry: costTelemetry({ inputTokens: null, outputTokens: null }, EXTERNAL_COST),
        evidence: [],
        summary:
          `Refused before running: ${chosenModel} is measured unusable for research here ` +
          `(${known.note}). Re-run with a ~27B model (see README), or answer it yourself.`,
      });
    }
    const dir = await mkdtemp(path.join(tmpdir(), "freellama-delegate-"));
    const promptFile = path.join(dir, "prompt.md");
    const resultFile = path.join(dir, "result.json");
    try {
      await writeFile(promptFile, `${question}\n`, "utf8");
      const adapter = RESEARCH_ADAPTERS[chosenAdapter];
      if (!existsSync(adapter)) {
        return researchErrorResult(
          new Error(
            `research adapter not found at ${adapter}. In a published install it should be bundled ` +
              "under <package>/adapters; in-repo it comes from benchmark/local/scripts. Reinstall, " +
              "or run `npm run build` from packages/mcp/ to re-copy it.",
          ),
        );
      }
      const running = execFileAsync("python3", [adapter], {
        signal: extra.signal,
        env: {
          ...delegateEnvironment(),
          FREELLAMA_TARGET_MODEL: chosenModel,
          FREELLAMA_AGENT_MANAGED_ENDPOINT: endpoint ?? DEFAULT_SERVE_ENDPOINT,
          FREELLAMA_AGENT_EXECUTION_PREFERENCE: executionPreference ?? "auto",
          FREELLAMA_AGENT_MIN_PLACEMENT_EVIDENCE: minPlacementEvidence ?? "configured",
          FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR: DEFAULT_TOKEN_CALIBRATION_DIR,
          FREELLAMA_BENCH_WORKSPACE: resolvedWorkspace,
          FREELLAMA_BENCH_PROMPT: promptFile,
          FREELLAMA_AGENT_RESULT: resultFile,
          FREELLAMA_AGENT_MAX_TURNS: String(agent?.maxTurns ?? DEFAULT_DELEGATE_MAX_TURNS),
          ...(agent?.contextTokens !== undefined ? { FREELLAMA_AGENT_NUM_CTX: String(agent.contextTokens) } : {}),
          ...(agent?.outputTokens !== undefined ? { FREELLAMA_AGENT_NUM_PREDICT: String(agent.outputTokens) } : {}),
          ...(agent?.temperature !== undefined ? { FREELLAMA_AGENT_TEMPERATURE: String(agent.temperature) } : {}),
          ...(agent?.seed !== undefined ? { FREELLAMA_AGENT_SEED: String(agent.seed) } : {}),
          ...(agent?.think !== undefined ? { FREELLAMA_AGENT_THINK: String(agent.think) } : {}),
          ...(agent?.keepAlive !== undefined ? { FREELLAMA_AGENT_KEEP_ALIVE: agent.keepAlive } : {}),
          ...(agent?.requestTimeoutSeconds !== undefined ? { FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS: String(agent.requestTimeoutSeconds) } : {}),
          ...(agent?.toolTimeoutSeconds !== undefined ? { FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS: String(agent.toolTimeoutSeconds) } : {}),
        },
        timeout: DEFAULT_DELEGATE_TIMEOUT_SECONDS * 1000,
        // The answer is read from `resultFile`, never from stdout — but execFile's default 1 MB
        // maxBuffer still applies to the agent's own progress logging, and overflowing it kills
        // the subprocess and surfaces as a research failure with no explanation. Give the logs
        // room; nothing here is proportional to the size of the answer.
        maxBuffer: 32 * 1024 * 1024,
        // On timeout, don't negotiate: SIGTERM can be swallowed by a python process blocked in a
        // long model call, which would leave the child (and its VRAM) alive past the deadline the
        // caller was promised.
        killSignal: "SIGKILL",
      });
      liveDelegates.add(running.child);
      // The adapter exits non-zero for its *own* failures — the model returned prose instead of
      // JSON, the endpoint was unreachable — but it still writes result.json first, with the real
      // diagnosis in `final_answer`. Letting the exec rejection propagate replaced that diagnosis
      // with "Command failed: python3 …", which names the wrong layer and hides the evidence trail
      // showing how far the run actually got. Capture it and prefer the adapter's own account.
      let adapterError: unknown = null;
      try {
        await running;
      } catch (error) {
        adapterError = error;
      } finally {
        liveDelegates.delete(running.child);
      }
      let result: ReturnType<typeof parseAdapterResult>;
      try {
        // Read AND parse under one guard. A SIGKILL at the timeout can land mid-write, leaving a
        // truncated result.json — reading it succeeds and only the parse fails, so guarding the
        // read alone reported "Unexpected end of JSON input" and threw away the timeout diagnosis
        // that actually explains the run. A valid object missing `final_answer` is the same class
        // of failure: unusable, not "empty trail".
        result = parseAdapterResult(await readFile(resultFile, "utf8"));
      } catch {
        // No usable result file: a hard kill (the SIGKILL timeout above) or a crash before the
        // adapter could finish writing. Here the exec error genuinely is the best account.
        return researchErrorResult(
          adapterError ??
            new Error(
              "research adapter exited without writing a readable result file — it was killed " +
                `before it could report. Check that the model is loadable and that ${DEFAULT_DELEGATE_TIMEOUT_SECONDS}s ` +
                "is enough for this question.",
            ),
        );
      }
      // Surface the evidence trail (which tool, which path) so the orchestrator can spot-check
      // *how* the answer was reached without re-deriving it — verifier independence in practice,
      // not just in principle. A citable but unread-through answer is exactly the failure mode
      // task-delegation.md warns about for judgment-heavy tasks.
      // The two adapters describe their calls differently — octocode puts the tool under
      // `arguments.tool` with a path in `arguments.queries.path`, bash reports `shell` with the
      // command line in `arguments.command`. Normalize both so the evidence trail reads the same
      // whichever adapter ran.
      const evidence = result.tool_calls.map((call, index) => {
        const target = call.arguments?.queries?.path;
        const rawDetail = call.arguments?.command ?? null;
        const detail = rawDetail ? clipText(rawDetail, 400) : null;
        return {
          step: index + 1,
          tool: call.arguments?.tool ?? call.raw_name ?? "?",
          // The adapters record "ok" | "error" | "repeat" per call. Carrying it through is what
          // lets a reader tell a run that read three files from one that failed three commands —
          // indistinguishable in the trail before, and graded identically.
          status: call.status ?? "ok",
          path: target
            ? path.relative(resolvedWorkspace, target)
            : detail
              ? extractExistingWorkspacePath(detail, resolvedWorkspace)
              : null,
          detail,
          detail_truncated: rawDetail !== detail,
        };
      });
      const succeeded = evidence.filter((step) => step.status === "ok");
      const failed = evidence.length - succeeded.length;
      // Commands can be arbitrarily long and are not reasoning-relevant by themselves. Keep a
      // marked excerpt in both halves; the cited path and tool remain enough to spot-check source.
      const evidenceText = evidence
        .map(
          (step) =>
            `  ${step.step}. ${step.tool}${step.status === "ok" ? "" : ` [${step.status}]`}` +
            `${step.path ? ` -> ${step.path}` : ""}` +
            `${step.detail ? `: ${clipText(step.detail, 400)}` : ""}`,
        )
        .join("\n");
      // The compact machine-readable half. Two independent small-model callers asked for exactly
      // this shape — recommendation, why, citations — rather than parsing it back out of the prose.
      // Successful steps only: a failed command is not a citation for anything.
      // Citations point into `evidence` by step instead of repeating each command a second time.
      const citations = succeeded.map((step) => ({ step: step.step, tool: step.tool, path: step.path }));
      if (adapterError) {
        const diagnostic = `research adapter failed: ${result.final_answer}` +
          (evidenceText ? `\nEvidence collected before the failure:\n${evidenceText}` : "");
        const refusal = z.object({ receipt: z.object({ error: z.string() }).passthrough() })
          .safeParse(result.model_metadata?.transport_error);
        if (refusal.success) {
          return researchErrorResult(new Error(JSON.stringify(refusal.data.receipt)), diagnostic);
        }
        return researchErrorResult(new Error(diagnostic));
      }
      // Grade on what actually read something. A run of failed commands is ungrounded no matter
      // how many of them there were.
      const verification = assessDelegatedAnswer(question, succeeded.length, chosenModel);
      const summary =
        `Delegated answer ready: ${result.tool_calls.length} tool call(s)` +
        (failed > 0 ? `, ${failed} of which did not succeed` : "") +
        `; ${result.usage.input_tokens ?? "?"} input / ${result.usage.output_tokens ?? "?"} output local tokens; ` +
        `verification=${verification.recommendation}. Read structuredContent.answer and citations.`;
      const payload = {
          adapter: chosenAdapter,
          verification,
          answer: result.final_answer,
          // recommendation + why live under `verification`; `citations` completes the triple so a
          // caller never has to read `summary` to act on the result.
          citations,
          toolCallCount: result.tool_calls.length,
          successfulToolCallCount: succeeded.length,
          usage: {
            inputTokens: result.usage.input_tokens,
            outputTokens: result.usage.output_tokens,
          },
          telemetry: costTelemetry({
            inputTokens: result.usage.input_tokens ?? null,
            outputTokens: result.usage.output_tokens ?? null,
          }, EXTERNAL_COST),
          contextManagement: result.model_metadata?.context_management ?? null,
          execution: {
            preference: executionPreference ?? "auto",
            minPlacementEvidence: minPlacementEvidence ?? "configured",
            receipts: result.model_metadata?.execution_receipts ?? [],
          },
          evidence,
          summary,
        };
      // The answer itself goes in TextContent: clients that forward only text to the model
      // otherwise received a pointer to structuredContent and never the answer.
      const cited = [...new Set(citations.map((citation) => citation.path).filter(Boolean))];
      const answerText =
        `${clipText(result.final_answer, ANSWER_TEXT_MAX_CHARS)}\n\n` +
        `verification: ${verification.recommendation}` +
        (cited.length ? `\ncited: ${cited.slice(0, 20).join(", ")}` : "") +
        `\n(${chosenModel}, ${result.tool_calls.length} tool call(s))`;
      return structuredResult(payload, legacyText === true ? { legacyJson: true } : { text: answerText });
    } catch (error) {
      return researchErrorResult(error);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  },
);

const transport = new StdioServerTransport();
await server.connect(transport);
