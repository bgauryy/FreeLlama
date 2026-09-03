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
  DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS,
  assertAllowedWorkspace,
} from "./config.js";
import {
  doctor, machine, health, createSession, deleteSession, listModels, route, runTaskRequest, runTaskBatchRequest, SERVER_VERSION,
} from "./native.js";
import {
  ollamaFetch,
  ollamaPull,
  parseAdapterResult,
  endpointParam,
  ollamaEndpointParam,
  taskParam,
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
import { parseModelSearch, parseModelTags } from "./model-search.js";
import { MODEL_EVIDENCE, assessDelegatedAnswer } from "./delegate.js";

const execFileAsync = promisify(execFile);

type Page<T> = { items: T[]; returned: number; total: number; next_cursor: string | null };
const EXTERNAL_COST = configuredExternalCost();

/** Attach accounting after a successful managed response; the Rust layer remains provider-neutral. */
function withTaskTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!("structuredContent" in result)) return result;
  const payload = result.structuredContent as Record<string, unknown>;
  const metrics = payload.metrics as Record<string, unknown> | undefined;
  return structuredResult({
    ...payload,
    telemetry: costTelemetry({
      inputTokens: typeof metrics?.prompt_tokens === "number" ? metrics.prompt_tokens : null,
      outputTokens: typeof metrics?.output_tokens === "number" ? metrics.output_tokens : null,
      totalDurationNs: typeof metrics?.total_duration_ns === "number" ? metrics.total_duration_ns : null,
    }, EXTERNAL_COST),
  });
}

function withBatchTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!("structuredContent" in result)) return result;
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
  });
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

process.on("exit", killLiveDelegates);
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
  process.on(signal, () => {
    killLiveDelegates();
    process.exit(0);
  });
}


const INSTRUCTIONS = `caller owns task decomposition; operator owns endpoints, exact --cpu-model assignments, lifecycle.
Ollama plus the OS/driver run physical CPU/GPU.
Efficient loop: models{view:"installed"}, then models{view:"resident"}; doctor only for runtime diagnosis. Preview consequential work before executing.
delegate_research is only for narrow allowed-workspace research. Keep full diagnostics, raw embeddings, and long evidence out of active context; read freellama://docs/index on demand.
ask approval for one exact tag and reported size before ollama_manage; search or recommendation is never download permission.
run_task preview never executes; code_review aliases coding.
Use requiredCapabilities:["tools"] to preview tool eligibility; omit preview and supply the payload to execute.
Docs: freellama://docs/index.`;

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
    description: "Use when: runtime/config diagnosis. Do not use when: model selection. Returns: compact summary by default; config/full are opt-in.",
    inputSchema: {
      endpoint: ollamaEndpointParam,
      serveEndpoint: endpointParam,
      view: z.enum(["summary", "scheduler", "config", "full"]).optional().describe("summary default; scheduler/config/full are verbose"),
    },
    outputSchema: doctorResultSchema,
    annotations: { readOnlyHint: true },
  },
  async ({ endpoint, serveEndpoint, view }) => {
    try {
      const report = parsedResult(await doctor(endpoint));
      if (!("structuredContent" in report)) return report;
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
      "Use when: retaining model affinity. Do not use when: storing history or KV. " +
      "Inputs: action, sessionId for delete. Returns: affinity handle or confirmation.",
    inputSchema: {
      action: z.enum(["create", "delete"]).describe("create | release"),
      sessionId: z.string().uuid().optional().describe("delete only"),
      endpoint: endpointParam,
    },
    outputSchema: sessionResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ action, sessionId, endpoint }) => {
    try {
      if (action === "create") {
        if (sessionId !== undefined) return errorResult(new Error("sessionId is only valid for action: delete."));
        return structuredResult(JSON.parse(await createSession(endpoint)));
      }
      if (sessionId === undefined) return errorResult(new Error("action: delete requires sessionId."));
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
      "Use when: inspecting models. Do not use when: executing or changing state; search never " +
      "permits a pull. Inputs: one view and its fields. Returns: inventory/detail, placement, or candidates. " +
      "Next: library family, then model:\"<family>\" for tags/fit.",
    inputSchema: {
      view: z
        .enum(["installed", "resident", "detail", "raw", "library"])
        .optional()
        .describe('installed (default, needs serve) | resident (needs serve) | detail | raw | library'),
      model: z.string().min(1).optional().describe('required for view "detail"; for "library", step 2 family name'),
      includeVerbose: z
        .boolean()
        .optional()
        .describe('"detail" only. Adds license/modelfile — the bulk of that payload, never routing-relevant'),
      query: z.string().min(1).optional().describe('"library" step 1: free text, e.g. "qwen", "embed"'),
      capabilities: z
        .array(z.enum(["vision", "tools", "thinking", "embedding", "cloud"]))
        .min(1)
        .max(5)
        .optional()
        .describe('"library" step 1: filter chips; combined as AND by the site'),
      order: z
        .enum(["popular", "newest"])
        .optional()
        .describe('"library" step 1. default "popular" — prefer it'),
      limit: z.number().int().positive().max(50).optional().describe('"library" search or raw/tag page size; raw default 20'),
      cursor: z.string().min(1).optional().describe('opaque continuation cursor for raw models or library tags'),
      endpoint: endpointParam,
      ollamaEndpoint: ollamaEndpointParam,
    },
    outputSchema: modelsResultSchema,
    annotations: { readOnlyHint: true },
  },
  async ({ view, model, includeVerbose, query, capabilities, order, limit, cursor, endpoint, ollamaEndpoint }) => {
    try {
      const selectedView = view ?? "installed";
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
          if (!("structuredContent" in managed)) return managed;
          const data = managed.structuredContent as {
            models?: Array<Record<string, unknown>>;
          };
          // Ollama's own docs say to check the GPU/CPU split, but /api/ps exposes only the raw
          // `size`/`size_vram` bytes it is derived from. The CLI computes it; the API doesn't.
          const models = (data.models ?? []).filter((entry) => entry.resident === true).map((entry) => {
            const size = typeof entry.size === "number" ? entry.size : null;
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
            ...(includeVerbose ? { license, modelfile } : {}),
          });
        }

        case "library":
          return await libraryLookup({ model, query, capabilities, order, limit, cursor, endpoint, ollamaEndpoint });

        default:
          return parsedResult(await listModels(endpoint));
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
  endpoint,
  ollamaEndpoint,
}: {
  model?: string;
  query?: string;
  capabilities?: Array<"vision" | "tools" | "thinking" | "embedding" | "cloud">;
  order?: "popular" | "newest";
  limit?: number;
  cursor?: string;
  endpoint?: string;
  ollamaEndpoint?: string;
}) {
    const fetchPage = async (url: string) => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000);
      try {
        const response = await fetch(url, { signal: controller.signal });
        if (!response.ok) throw new Error(`ollama.com returned HTTP ${response.status}`);
        return await response.text();
      } finally {
        clearTimeout(timer);
      }
    };
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
        const family = model.split(":")[0];
        const familyPage = await fetchPage(`https://ollama.com/library/${encodeURIComponent(family)}`);
        const { tags } = parseModelTags(familyPage, family);
        const { installed, memoryBytes } = await localState();
        const budget = memoryBytes ? memoryBytes * 0.6 : null;
        const annotated = tags.map((t) => ({
          ...t,
          installed: installed.has(t.tag),
          fitsInMemory: budget && t.sizeBytes ? t.sizeBytes <= budget : null,
          fitScope: "host_memory_budget_only",
        }));
        const runnable = annotated.filter((t) => t.fitsInMemory === true && !/cloud/.test(t.tag));
        const embeddingMentions = (familyPage.match(/embedding/gi) ?? []).length;
        const isEmbeddingFamily = embeddingMentions >= 3 || /embed/i.test(family);
        const sized = runnable.filter((t) => t.sizeBytes);
        const best = budget
          ? isEmbeddingFamily
            ? sized.sort((a, b) => (a.sizeBytes ?? 0) - (b.sizeBytes ?? 0))[0]
            : sized.sort((a, b) => (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0))[0]
          : undefined;
        const tagPage = pageLiveList(annotated, limit, cursor, (tag) => tag.tag);
        return structuredResult({
          family,
          sourcePage: `https://ollama.com/library/${encodeURIComponent(family)}`,
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
          recommendation: best
            ? isEmbeddingFamily
              ? {
                  tag: best.tag,
                  why:
                    "Smallest tag that fits — this is an embedding family, where bigger is NOT " +
                    "better. Measured on real retrieval over this repo: nomic-embed-text (274MB) " +
                    "scored 5/6 recall@3 in 4.2s, while qwen3-embedding:0.6b (2.3x the size) " +
                    "scored 4/6 at 3.5x the indexing cost. Embeddings do no sampling, so there is " +
                    "no accuracy cliff for size to buy you past.",
                  configure:
                    'Batch your inputs in one `run_task` call and use keepAlive:"0" — an embedding ' +
                    "model has no reason to hold memory after the vectors are computed. Leave " +
                    "returnEmbeddings off unless you are storing the vectors yourself.",
                  caution:
                    "Index once, query many times, and own the staleness: FreeLlama stores no " +
                    "vectors, and a stale index fails silently by returning confidently wrong " +
                    "files. Also, if you know the keyword, grep beat embedding search here on " +
                    "accuracy, latency and cost at the same time — reach for embeddings when " +
                    "there is no keyword to search for.",
                }
              : {
                  tag: best.tag,
                  why:
                    `Largest tag inside the conservative ~60% host-memory preflight${memoryBytes ? ` of ${Math.round(memoryBytes / 1e9)}GB` : ""}. ` +
                    "This proves neither accelerator fit nor task quality; preview and benchmark before pulling.",
                  configure:
                    "Send an explicit num_ctx rather than inheriting Ollama's default, which is " +
                    '4096 tokens by default unless the Ollama service or request sets num_ctx. Use keepAlive:"0" for one-off ' +
                    "calls so it does not hold memory after.",
                  caution:
                    "Do not co-resident large models until their combined runner sizes, contexts, " +
                    "KV caches, and OS headroom fit this host. Check `models{view:\"resident\"}` and " +
                    "accelerator telemetry first; benchmark packaging rather than inferring quality " +
                    "or speed from a model suffix.",
                }
            : null,
        });
      }

      const params = new URLSearchParams();
      for (const c of capabilities ?? []) params.append("c", c);
      if (query) params.set("q", query);
      if (order === "newest") params.set("o", "newest");
      const url = `https://ollama.com/search?${params.toString()}`;
      const parsed = parseModelSearch(await fetchPage(url)).slice(0, limit ?? 10);
      const { installed } = await localState();
      const installedFamilies = new Set([...installed].map((n) => n.split(":")[0]));
      return structuredResult({
        query: url,
        order: order ?? "popular",
        count: parsed.length,
        nextStep: 'Not pullable yet. Call again with view:"library", model:"<name>" to get tags, sizes and memory fit.',
        models: parsed.map((m) => ({ ...m, installed: installedFamilies.has(m.name) })),
      });
    } catch (error) {
      return errorResult(error);
    }
}

server.registerTool(
  "run_task",
  {
    description:
      "Use when: routing or executing supplied content. Do not use when: workspace files must be read; use delegate_research. " +
      "Preview consequential work first; it never generates. Returns: decision or response with receipts. Next: inspect structured observation and verification.",
    inputSchema: {
      endpoint: endpointParam,
      task: taskParam,
      objective: objectiveParam,
      model: z.string().min(1).optional().describe("Force this exact installed model name."),
      sessionId: z.string().min(1).optional().describe("Session id for model affinity across calls."),
      contextTokens: z.number().int().positive().optional().describe("Total Ollama context window (num_ctx), including input and output."),
      executionPreference: executionPreferenceParam,
      minPlacementEvidence: minPlacementEvidenceParam,
      requiredCapabilities: requiredCapabilitiesParam,
      prompt: z.string().min(1).optional().describe("Chat input when messages is omitted."),
      images: z
        .array(z.string().min(1))
        .min(1)
        .optional()
        .describe("base64, no data-URI prefix; prompt mode only; requires an explicit tested vision model"),
      messages: z
        .array(
          z
            .object({ role: z.enum(["system", "user", "assistant", "tool"]), content: z.string() })
            .passthrough(),
        )
        .min(1)
        .optional()
        .describe(
          "wins over prompt; preserves Ollama images, thinking, tool_calls, tool_name, and other message fields",
        ),
      input: z
        .union([z.string().min(1), z.array(z.string().min(1)).min(1)])
        .optional()
        .describe('embedding only; batch strings because one call is far cheaper than one call per item'),
      tools: z.array(z.record(z.unknown())).min(1).optional().describe("Ollama function definitions for chat tasks."),
      keepAlive: z.string().min(1).optional().describe('"0" unloads now, "-1" pins, default 5m'),
      format: z
        .union([z.literal("json"), z.record(z.unknown())])
        .optional()
        .describe('Ollama structured output: "json" or a JSON schema object'),
      think: z
        .union([z.boolean(), z.enum(["low", "medium", "high"])])
        .optional()
        .describe("Override the task profile for thinking-capable models"),
      options: z
        .record(z.unknown())
        .optional()
        .describe("Advanced Ollama options; num_ctx/contextTokens and num_gpu/placement are routing-owned. num_predict caps output."),
      logprobs: z.boolean().optional(),
      topLogprobs: z.number().int().nonnegative().optional().describe("Requires logprobs:true"),
      minConfidence: minConfidenceParam,
      priority: z.enum(["interactive", "normal", "background"]).optional().describe("Admission class only; normal default. Fair scheduling prevents background starvation."),
      returnEmbeddings: z.boolean().optional().describe("false (default) withholds the raw vectors; they are large and unreadable to a model"),
      preview: z
        .boolean()
        .optional()
        .describe(
          "true = routing fields only; rejects payloads and runtime controls; never executes",
        ),
    },
    outputSchema: taskResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({
    endpoint,
    task,
    objective,
    model,
    sessionId,
    contextTokens,
    executionPreference,
    minPlacementEvidence,
    requiredCapabilities,
    prompt,
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
    returnEmbeddings,
    preview,
  }) => {
    try {
      const canonicalTask = canonicalTaskKind(task);
      if (preview) {
        const executionOnlyFields = ([
          ["prompt", prompt],
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
          [prompt, images, messages, tools, format, think, logprobs, topLogprobs].some(
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
        if (prompt === undefined && (messages === undefined || messages.length === 0)) {
          return errorResult(new Error(`task "${task}" requires \`prompt\` or \`messages\`.`));
        }
        if (images !== undefined && messages !== undefined) {
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
        if ("structuredContent" in result) {
          const refusal = belowConfidence(result.structuredContent, minConfidence);
          if (refusal) return refusal;
        }
        return result;
      }
      // Gating after the fact would be useless here: by the time `run_task` returns, the tokens
      // are spent. So when the caller sets a floor, preview the decision with a `route` call
      // first — free, no generation — and refuse before anything runs. Only costs the extra round
      // trip when the option is actually used.
      if (minConfidence) {
        const decision = parsedResult(
          // minConfidence is forwarded so the CORE gate refuses, with its actionable message naming
        // the two commands that raise the grade. The belowConfidence() check below stays only as a
          // fallback for servers older than the core gate.
        await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
        );
        if ("structuredContent" in decision) {
          const refusal = belowConfidence(decision.structuredContent, minConfidence);
          if (refusal) return refusal;
        }
      }
      const result = parsedResult(
        await runTaskRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
          task: canonicalTask,
          objective: objective ?? "balanced",
          model,
          session_id: sessionId,
          context_tokens: contextTokens,
          required_capabilities: effectiveRequiredCapabilities ?? [],
          prompt,
          images,
          messages: messages ?? [],
          input,
          tools,
          keep_alive: keepAlive,
          min_confidence: minConfidence,
          priority: priority ?? "normal",
          execution_preference: executionPreference ?? "auto",
          min_placement_evidence: minPlacementEvidence ?? "configured",
          request_options: {
            format,
            think,
            options,
            logprobs,
            top_logprobs: topLogprobs,
          },
        }),
      );
      if (!returnEmbeddings && "structuredContent" in result) {
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
  "run_task_batch",
  {
    description:
      "Use when: independent work. Do not use when: dependencies. " +
      "Inputs: [{id, independent:true, task}]; maxParallelism caps dispatch. Returns: ordered receipts.",
    inputSchema: {
      tasks: z.array(batchItemParam).min(1).max(64),
      maxParallelism: z.number().int().positive().max(64).optional(),
      endpoint: endpointParam,
    },
    outputSchema: batchResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ tasks, maxParallelism, endpoint }) => {
    try {
      for (const item of tasks) {
        const task = item.task as Record<string, unknown>;
        if (task.task === "embedding") {
          if (task.input === undefined || task.prompt !== undefined || task.messages !== undefined || task.tools !== undefined) {
            return errorResult(new Error(`batch item ${item.id}: embedding requires input and accepts no chat payload.`));
          }
        } else if (task.input !== undefined || (task.prompt === undefined && task.messages === undefined)) {
          return errorResult(new Error(`batch item ${item.id}: chat work requires prompt or messages and accepts no input.`));
        }
        if (task.images !== undefined && task.model === undefined) {
          return errorResult(new Error(`batch item ${item.id}: images require an explicit tested vision model.`));
        }
      }
      return withBatchTelemetry(parsedResult(await runTaskBatchRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
        tasks: tasks.map((item) => {
          const task = item.task as Record<string, unknown>;
          return ({
          id: item.id,
          independent: item.independent,
          task: {
            task: task.task === "code_review" ? "coding" : task.task,
            objective: (task as Record<string, unknown>).objective ?? "balanced",
            model: task.model,
            session_id: (task as Record<string, unknown>).sessionId,
            context_tokens: (task as Record<string, unknown>).contextTokens,
            execution_preference: (task as Record<string, unknown>).executionPreference ?? "auto",
            min_placement_evidence: (task as Record<string, unknown>).minPlacementEvidence ?? "configured",
            required_capabilities: (task as Record<string, unknown>).requiredCapabilities ?? [],
            priority: task.priority ?? "normal",
            prompt: task.prompt,
            images: task.images,
            messages: task.messages ?? [],
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
      "Use when: pulling an approved tag or stopping a loaded model. Do not use when: deleting/recommending. " +
      "Inputs: action/model; timeoutSeconds is pull-only. Returns: lifecycle result. Next: inspect models.",
    inputSchema: {
      action: z.enum(["pull", "stop"]).describe('"pull" = disk, "stop" = memory'),
      model: z.string().min(1),
      ollamaEndpoint: ollamaEndpointParam,
      timeoutSeconds: z
        .number()
        .int()
        .positive()
        .optional()
        .describe(`"pull" only. Defaults to ${DEFAULT_PULL_TIMEOUT_SECONDS}s.`),
    },
    outputSchema: manageResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ action, model, ollamaEndpoint, timeoutSeconds }) => {
    try {
      if (action === "stop" && timeoutSeconds !== undefined) {
        return errorResult(new Error('`timeoutSeconds` is valid only for action "pull".'));
      }
      const data =
        action === "pull"
          ? await ollamaPull(ollamaEndpoint, model, timeoutSeconds)
          : await ollamaFetch(ollamaEndpoint, "/api/generate", {
              method: "POST",
              body: { model, keep_alive: 0 },
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
      "DESTRUCTIVE AND IRREVERSIBLE. Use when: a human requests one exact tag. Do not use when: freeing memory, " +
      "cleaning by age, or inferring a tag. Inputs: exact tag. Returns: deleted tag. Next: refresh models.",
    inputSchema: {
      model: z.string().min(1),
      ollamaEndpoint: ollamaEndpointParam,
    },
    outputSchema: deleteResultSchema,
    annotations: { destructiveHint: true },
  },
  async ({ model, ollamaEndpoint }) => {
    try {
      await ollamaFetch(ollamaEndpoint, "/api/delete", {
        method: "DELETE",
        body: { name: model },
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
server.registerTool(
  "delegate_research",
  {
    description:
      "Use when: a narrow lookup needs an allowed workspace. Do not use when: mutation, broad judgment, or outside-workspace facts are needed. " +
      "Pages tool evidence instead of flooding caller context. Returns: answer, citations, evidence, verdict. Next: verify or escalate yourself.",
    inputSchema: {
      question: z.string().min(1).describe("narrow and self-contained, answerable by reading files"),
      workspacePath: z
        .string()
        .min(1)
        .describe("directory inside FREELLAMA_MCP_ALLOWED_ROOTS (absolute preferred; relative is resolved)"),
      adapter: z
        .enum(["bash", "octocode"])
        .optional()
        .describe('"bash" (default) beat "octocode" on every model measured, and is faster'),
      model: z
        .string()
        .min(1)
        .optional()
        .describe("omit to use FREELLAMA_MCP_DEFAULT_MODEL; research accuracy collapses below ~12B"),
      endpoint: endpointParam,
      executionPreference: executionPreferenceParam,
      minPlacementEvidence: minPlacementEvidenceParam,
      legacyText: z.boolean().optional().describe("false default: compact text cue; true: legacy serialized JSON text"),
      agent: z
        .object({
          maxTurns: z.number().int().positive().optional(),
          contextTokens: z.number().int().positive().optional().describe("Total agent context; input reserves outputTokens and safety margin."),
          outputTokens: z.number().int().positive().optional().describe("Maximum generated tokens per model call; reserved from context."),
          temperature: z.number().nonnegative().optional(),
          seed: z.number().int().nonnegative().optional(),
          think: z.boolean().optional(),
          keepAlive: z.string().min(1).optional().describe('Ollama duration such as "5m", "0", or "-1"'),
          requestTimeoutSeconds: z.number().positive().optional(),
          toolTimeoutSeconds: z.number().positive().optional(),
          retryAttempts: z.number().int().positive().optional(),
          retryBackoffSeconds: z.number().nonnegative().optional(),
          maxParseRepairs: z.number().int().nonnegative().optional(),
          parseRepairEchoChars: z.number().int().positive().optional(),
          context: z
            .object({
              charsPerToken: z.number().positive().optional(),
              safetyMarginTokens: z.number().int().nonnegative().optional(),
              imageTokenEstimate: z.number().int().nonnegative().optional(),
              keepRecent: z.number().int().nonnegative().optional(),
              compactPreviewChars: z.number().int().positive().optional(),
              compactRetainRatio: z.number().positive().lt(1).optional(),
              clipHeadRatio: z.number().positive().lt(1).optional(),
              observationPageChars: z.number().int().positive().optional(),
              pinnedOverflow: z.enum(["error", "clip"]).optional(),
            })
            .optional(),
        })
        .optional()
        .describe("Per-call agent budget, recovery, and compaction controls."),
    },
    outputSchema: researchResultSchema,
    annotations: { destructiveHint: false },
  },
  async ({ question, workspacePath, adapter, model, endpoint, executionPreference, minPlacementEvidence, legacyText, agent }) => {
    const chosenAdapter: ResearchAdapter = adapter ?? DEFAULT_RESEARCH_ADAPTER;
    const chosenModel = model ?? DEFAULT_DELEGATE_MODEL;
    let resolvedWorkspace: string;
    try {
      resolvedWorkspace = await assertAllowedWorkspace(workspacePath);
    } catch (error) {
      return errorResult(error);
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
        return errorResult(
          new Error(
            `research adapter not found at ${adapter}. In a published install it should be bundled ` +
              "under <package>/adapters; in-repo it comes from benchmark/local/scripts. Reinstall, " +
              "or run `npm run build` from packages/mcp/ to re-copy it.",
          ),
        );
      }
      const running = execFileAsync("python3", [adapter], {
        env: {
          ...process.env,
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
          ...(agent?.retryAttempts !== undefined ? { FREELLAMA_AGENT_RETRY_ATTEMPTS: String(agent.retryAttempts) } : {}),
          ...(agent?.retryBackoffSeconds !== undefined ? { FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS: String(agent.retryBackoffSeconds) } : {}),
          ...(agent?.maxParseRepairs !== undefined ? { FREELLAMA_AGENT_MAX_PARSE_REPAIRS: String(agent.maxParseRepairs) } : {}),
          ...(agent?.parseRepairEchoChars !== undefined ? { FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS: String(agent.parseRepairEchoChars) } : {}),
          ...(agent?.context?.charsPerToken !== undefined ? { FREELLAMA_AGENT_CHARS_PER_TOKEN: String(agent.context.charsPerToken) } : {}),
          ...(agent?.context?.safetyMarginTokens !== undefined ? { FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS: String(agent.context.safetyMarginTokens) } : {}),
          ...(agent?.context?.imageTokenEstimate !== undefined ? { FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE: String(agent.context.imageTokenEstimate) } : {}),
          ...(agent?.context?.keepRecent !== undefined ? { FREELLAMA_AGENT_KEEP_RECENT: String(agent.context.keepRecent) } : {}),
          ...(agent?.context?.compactPreviewChars !== undefined ? { FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS: String(agent.context.compactPreviewChars) } : {}),
          ...(agent?.context?.compactRetainRatio !== undefined ? { FREELLAMA_AGENT_COMPACT_RETAIN_RATIO: String(agent.context.compactRetainRatio) } : {}),
          ...(agent?.context?.clipHeadRatio !== undefined ? { FREELLAMA_AGENT_CLIP_HEAD_RATIO: String(agent.context.clipHeadRatio) } : {}),
          ...(agent?.context?.observationPageChars !== undefined ? { FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS: String(agent.context.observationPageChars) } : {}),
          ...(agent?.context?.pinnedOverflow !== undefined ? { FREELLAMA_AGENT_PINNED_OVERFLOW: agent.context.pinnedOverflow } : {}),
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
        return errorResult(
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
      const citations = succeeded.map((step) => ({
        step: step.step,
        tool: step.tool,
        path: step.path,
        command: step.detail,
        command_truncated: step.detail_truncated,
      }));
      if (adapterError) {
        return errorResult(
          new Error(
            `research adapter failed: ${result.final_answer}` +
              (evidenceText ? `\nEvidence collected before the failure:\n${evidenceText}` : ""),
          ),
        );
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
      return structuredResult(payload, { legacyJson: legacyText === true });
    } catch (error) {
      return errorResult(error);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  },
);

const transport = new StdioServerTransport();
await server.connect(transport);
