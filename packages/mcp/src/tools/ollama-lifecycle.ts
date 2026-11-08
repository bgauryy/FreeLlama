// Model lifecycle: pull and delete talk to Ollama directly; warm goes through serve's managed
// admission; unload goes through serve when it can (it waits for running tasks), else Ollama.
// `models` covers list/ps/show. `ollama_pull` is its own tool so a download needs the size the user
// approved and is the only lifecycle call that reaches the internet; `ollama_delete` is its own tool
// so the others are not marked destructive.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_PULL_TIMEOUT_SECONDS } from "../config.js";
import {
  endpointParam, errorResult, executionPreferenceParam, messagesParam, minPlacementEvidenceParam, ollamaEndpointParam,
  isStructuredSuccess, lifecycleText, ollamaFetch, parsedResult, structuredResult, systemPromptParam, taskMessages, taskParam,
} from "../helpers.js";
import { runWithJobProgress } from "../job-progress.js";
import { progressSink, type ToolExtra } from "../progress.js";
import { serveUnload } from "../serve-http.js";
import { installedMatch, type InstalledModel } from "../model-catalog.js";
import { installedInventory, libraryTagSize, type TagSizeLookup } from "../model-library.js";

const manageResultSchema = z.object({
  status: z.string().optional(), progress: z.unknown().optional(),
}).loose();
const deleteResultSchema = z.object({
  deleted: z.string(),
}).loose();

/**
 * Collapse Ollama `/api/pull` NDJSON into one object an agent can read.
 *
 * `stream: false` hid every byte of a multi-GB download behind a blocking wait. Each NDJSON
 * line is a status snapshot (`pulling manifest`, layer digest, `completed`/`total`). Keep the
 * last line and the last line that had a byte total so the tool result reports percent done
 * (or `success` for an already-installed tag) instead of a silent hang.
 */
export function summarizeOllamaPullStream(text: string): Record<string, unknown> {
  const events: Record<string, unknown>[] = [];
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        events.push(parsed as Record<string, unknown>);
      }
    } catch {
      // Non-JSON noise on the stream is ignored; the last valid event is the result.
    }
  }
  if (events.length === 0) {
    if (!text.trim()) return { status: "empty" };
    try {
      const parsed = JSON.parse(text) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return parsed as Record<string, unknown>;
      }
    } catch {
      return { status: "unparsed", raw: text };
    }
    return { status: "unparsed", raw: text };
  }
  const last = events[events.length - 1] ?? {};
  const withBytes = [...events]
    .reverse()
    .find((event) => typeof event.total === "number" && (event.total as number) > 0);
  const completed = typeof withBytes?.completed === "number" ? (withBytes.completed as number) : null;
  const total = typeof withBytes?.total === "number" ? (withBytes.total as number) : null;
  const percent =
    completed != null && total != null && total > 0
      ? Math.round((completed / total) * 1000) / 10
      : null;
  // Ollama reports failures as an {"error": ...} event on an HTTP 200 stream (verified live: a
  // bogus tag yields `pulling manifest` then `{"error":"pull model manifest: ..."}`), so nothing
  // at the transport layer fails. Without this, a failed pull came back success-shaped.
  const status = typeof last.error === "string" && last.error ? "error" : last.status;
  return {
    ...last,
    ...(status === undefined ? {} : { status }),
    progress: {
      events: events.length,
      completed,
      total,
      percent,
      lastStatus: last.status ?? null,
    },
  };
}

export async function ollamaPull(
  endpoint: string | undefined,
  model: string,
  timeoutSeconds?: number,
  options: { signal?: AbortSignal; onProgress?: (event: Record<string, unknown>) => void } = {},
): Promise<Record<string, unknown>> {
  const raw = await ollamaFetch(endpoint, "/api/pull", {
    method: "POST",
    body: { model, stream: true },
    timeoutMs: (timeoutSeconds ?? DEFAULT_PULL_TIMEOUT_SECONDS) * 1000,
    parse: "ndjson",
    signal: options.signal,
    onLine: options.onProgress,
  });
  if (typeof raw === "string") return summarizeOllamaPullStream(raw);
  if (raw && typeof raw === "object" && !Array.isArray(raw)) return raw as Record<string, unknown>;
  return { status: "empty" };
}



/** A monotonic notification counter; layer byte totals describe the message, not overall progress. */
export function pullProgressReporter(extra: ToolExtra): ((event: Record<string, unknown>) => void) | undefined {
  const report = progressSink(extra);
  if (!report) return undefined;
  let lastSent = -Infinity;
  return (event) => {
    const now = Date.now();
    if (now - lastSent < 500 && event.status !== "success") return;
    lastSent = now;
    const completed = typeof event.completed === "number" && Number.isFinite(event.completed) && event.completed >= 0 ? event.completed : undefined;
    const total = typeof event.total === "number" && Number.isFinite(event.total) && event.total > 0 ? event.total : undefined;
    const percent = completed !== undefined && total !== undefined && completed <= total
      ? ` (${Math.round(completed / total * 1000) / 10}%)` : "";
    const bytes = completed !== undefined ? `: ${completed}${total === undefined ? "" : `/${total}`} bytes${percent}` : "";
    report(`${String(event.status ?? "pulling")}${bytes}`);
  };
}

/**
 * Unload one model. With the default Ollama endpoint, serve does it: it waits for that model's
 * running tasks and records the unload. Ollama is asked directly when a caller names another
 * Ollama endpoint, or serve is unreachable or too old to have `/unload`.
 */
export async function unloadModel(
  model: string,
  ollamaEndpoint: string | undefined,
  signal?: AbortSignal,
  unloadThroughServe: typeof serveUnload = serveUnload,
  endpoint?: string,
): Promise<Record<string, unknown>> {
  if (ollamaEndpoint === undefined) {
    const served = await unloadThroughServe(model, { endpoint, signal });
    if (served) return { ...served, via: "freellama_serve" };
  }
  const payload = await ollamaFetch(ollamaEndpoint, "/api/generate", {
    method: "POST",
    body: { model, keep_alive: 0 },
    signal,
  }) as Record<string, unknown>;
  if (typeof payload.error === "string" && payload.error) return payload;
  // Ollama accepted the unload; it does not report whether the runner is gone yet.
  return { ...payload, model, unloaded: payload.done === true, status: "unload_requested", via: "ollama" };
}

/** How far the size the user approved may drift from the one the library reports now. */
export const PULL_SIZE_TOLERANCE = 0.05;

/** What `ollama_pull` reads and does; tests replace them, the server uses the defaults. */
export type PullDependencies = {
  lookupTagSize: (model: string) => Promise<TagSizeLookup>;
  installed: (ollamaEndpoint: string | undefined) => Promise<InstalledModel[]>;
  pull: typeof ollamaPull;
};

const defaultPullDependencies: PullDependencies = { lookupTagSize: libraryTagSize, installed: installedInventory, pull: ollamaPull };

const DEPRECATED_PULL = 'ollama_manage {action:"pull"} no longer downloads. Call ollama_pull {model:"<exact tag>", approvedSizeBytes:<the size the user approved>} ' +
  'with the tag and size from models{view:"library", model:"<family>"} that the user approved.';

const sizeText = (bytes: number) => `${bytes} bytes (${(bytes / 1e9).toFixed(2)} GB)`;

export function registerOllamaLifecycleTools(server: McpServer, pullDependencies: PullDependencies = defaultPullDependencies): void {
  server.registerTool(
    "ollama_pull",
    {
      description:
        "Use when: the user approved downloading one exact tag at its reported size (models{view:\"library\", model:<family>} lists tags with sizeBytes). " +
        "Pass that size as approvedSizeBytes: the pull is refused when the tag is unknown or its current size differs by more than 5%. " +
        "Do not use when: you only found or recommended a model (search is not approval), or the tag is installed. Returns: a receipt with status and progress.",
      inputSchema: z.object({
        model: z.string().min(1).describe("exact tag, e.g. family:tag or hf.co/owner/repo:quant"),
        approvedSizeBytes: z.number().int().positive().describe("the download size the user approved, in bytes"),
        ollamaEndpoint: ollamaEndpointParam,
        timeoutSeconds: z.number().int().positive().optional(),
      }).strict(),
      outputSchema: manageResultSchema,
      // Downloads from the internet (openWorldHint, whose spec default is true, so it is not restated);
      // additive, and pulling an installed tag again changes nothing.
      annotations: { destructiveHint: false, idempotentHint: true },
    },
    async ({ model, approvedSizeBytes, ollamaEndpoint, timeoutSeconds }, extra) => {
      try {
        // A bare family name resolves to `:latest`, whose size the user never saw.
        if (!/:[^/:]+$/.test(model)) {
          return errorResult(new Error(`ollama_pull needs an exact tag such as ${model}:<tag>; list tags and sizes with models{view:"library", model:"${model}"} and ask the user to approve one.`));
        }
        let lookup: TagSizeLookup;
        try {
          lookup = await pullDependencies.lookupTagSize(model);
        } catch (error) {
          // Offline or the source is down: the approved size cannot be checked, so nothing is pulled.
          const reason = error instanceof Error ? error.message : String(error);
          let installed: InstalledModel[] | null = null;
          try { installed = await pullDependencies.installed(ollamaEndpoint); } catch { /* reported below */ }
          if (installed && installedMatch(model, installed).installed) {
            const payload = { status: "already_installed", model, pulled: false, sizeCheck: `unavailable: ${reason}` };
            return structuredResult(payload, { text: `${model}: already installed; not pulled (the size lookup failed: ${reason}).` });
          }
          return errorResult(new Error(
            `Not pulled: the size of ${model} could not be looked up (${reason}), so the approved ${sizeText(approvedSizeBytes)} cannot be checked. ` +
            `Retry when ollama.com or Hugging Face is reachable${installed === null ? "; the local inventory could not be read either" : ""}.`,
          ));
        }
        if (!lookup.found) {
          return errorResult(new Error(`Not pulled: ${lookup.message} Check the exact tag with models{view:"library"} and re-ask the user to approve a listed tag and its size.`));
        }
        if (lookup.sizeBytes === null) {
          return errorResult(new Error(`Not pulled: ${lookup.tag} has no reported download size${lookup.sourcePage ? ` on ${lookup.sourcePage}` : ""}, so the approved ${sizeText(approvedSizeBytes)} cannot be checked.`));
        }
        if (Math.abs(lookup.sizeBytes - approvedSizeBytes) > lookup.sizeBytes * PULL_SIZE_TOLERANCE) {
          return errorResult(new Error(
            `Not pulled: ${lookup.tag} is reported at ${sizeText(lookup.sizeBytes)}, but the approved size is ${sizeText(approvedSizeBytes)} ` +
            `(more than ${PULL_SIZE_TOLERANCE * 100}% apart). Re-ask the user to approve ${lookup.tag} at ${sizeText(lookup.sizeBytes)}.`,
          ));
        }
        const payload = await pullDependencies.pull(ollamaEndpoint, model, timeoutSeconds, {
          signal: extra.signal,
          onProgress: pullProgressReporter(extra),
        });
        // Pull failures arrive as an {"error": ...} event on an HTTP 200 stream, so the fetch
        // above succeeds; report them as errors or a caller gating on isError believes the model
        // is installed.
        if (typeof payload.error === "string" && payload.error) {
          return errorResult(new Error(`pull ${model} failed: ${payload.error}`));
        }
        return structuredResult({ ...payload, approvedSizeBytes, reportedSizeBytes: lookup.sizeBytes });
      } catch (error) {
        return errorResult(error);
      }
    },
  );

  server.registerTool(
    "ollama_manage",
    {
      description:
        "Use when: loading or freeing a model. warm loads an installed model before a burst and evaluates an optional systemPrompt/messages into the prompt cache; " +
        "unload frees memory (a model you warmed or the user named). " +
        "Do not use when: downloading (ollama_pull; action pull is deprecated and refused), deleting (ollama_delete) or searching (models). Returns: a receipt with status.",
      inputSchema: z.object({
        action: z.enum(["pull", "warm", "unload"]),
        model: z.string().min(1).describe("exact tag"),
        ollamaEndpoint: ollamaEndpointParam,
        endpoint: endpointParam.describe("serve endpoint (warm, unload)"),
        timeoutSeconds: z.number().int().positive().optional().describe("warm"),
        task: taskParam.removeDefault().exclude(["decision"]).optional().describe('warm: runner profile; embedding-only models need none, "embedding" for ones that also complete'),
        contextTokens: z.number().int().positive().optional().describe("warm: the num_ctx later calls use"),
        systemPrompt: systemPromptParam,
        messages: messagesParam,
        keepAlive: z.string().min(1).optional().describe('warm: "-1" pins; omitted: adaptive TTL'),
        executionPreference: executionPreferenceParam,
        minPlacementEvidence: minPlacementEvidenceParam,
        priority: z.enum(["interactive", "normal", "background"]).optional(),
        maxWaitSeconds: z.number().int().positive().optional().describe("capped by the operator's max_queue_wait_seconds (default 120); lowered waits show as wait_limit"),
        defer: z.boolean().optional(),
      }).strict(),
      outputSchema: manageResultSchema,
      // Unload ends another client's warm runner but deletes nothing, so the tool is not destructive.
      // Warm and unload are local now that pulling is ollama_pull; openWorldHint is left unset, as
      // on every tool here.
      annotations: { destructiveHint: false },
    },
    async ({ action, model, ollamaEndpoint, endpoint, timeoutSeconds, task, contextTokens, systemPrompt, messages,
      keepAlive, executionPreference, minPlacementEvidence, priority, maxWaitSeconds, defer }, extra) => {
      try {
        // Deprecated alias, kept for one release so old callers get directions instead of a schema
        // error. It never downloads: a pull needs the approved size (ollama_pull).
        if (action === "pull") return errorResult(new Error(DEPRECATED_PULL));
        const warmFields = { task, contextTokens, systemPrompt, messages, keepAlive, executionPreference,
          minPlacementEvidence, priority, maxWaitSeconds, defer };
        const stray = Object.entries(warmFields).filter(([, value]) => value !== undefined).map(([name]) => `\`${name}\``);
        if (action !== "warm" && stray.length > 0) {
          return errorResult(new Error(`${stray.join(", ")} ${stray.length === 1 ? "applies" : "apply"} to action "warm" only.`));
        }
        if (action === "warm") {
          if (ollamaEndpoint !== undefined) return errorResult(new Error('warm runs through serve; pass `endpoint`, not `ollamaEndpoint`.'));
          const prefix = taskMessages({ systemPrompt, messages });
          // Loaded on use: the serve client needs the native addon, which unload does not.
          const { warmModelRequest, getTaskJob, cancelTaskJob } = await import("../serve-client.js");
          const request = {
            model, task, context_tokens: contextTokens, messages: prefix.length === 0 ? undefined : prefix,
            execution_preference: executionPreference, min_placement_evidence: minPlacementEvidence,
            priority, keep_alive: keepAlive, max_wait_seconds: maxWaitSeconds, timeout_seconds: timeoutSeconds, defer,
          };
          // A cold load can outlast the client's request timeout. Run it as a job so each phase
          // (queued, waiting for memory, loading) is reported and a client cancel stops it.
          const warmed = parsedResult(defer === true
            ? await warmModelRequest(endpoint, request)
            : await runWithJobProgress({
              submit: () => warmModelRequest(endpoint, { ...request, defer: true }),
              get: (id) => getTaskJob(endpoint, id),
              cancel: (id) => cancelTaskJob(endpoint, id),
              // Reached only when serve refused the job before running it (registry full).
              sync: () => warmModelRequest(endpoint, request),
            }, progressSink(extra) ?? (() => undefined), { signal: extra.signal }));
          // One line for the model; the receipt with its execution diagnostics stays structured.
          return isStructuredSuccess(warmed)
            ? structuredResult(warmed.structuredContent, { text: lifecycleText("warm", model, warmed.structuredContent) })
            : warmed;
        }
        if (timeoutSeconds !== undefined) {
          return errorResult(new Error('`timeoutSeconds` is valid only for action "warm" (and for ollama_pull).'));
        }
        const payload = await unloadModel(model, ollamaEndpoint, extra.signal, serveUnload, endpoint);
        if (typeof payload.error === "string" && payload.error) {
          return errorResult(new Error(`${action} ${model} failed: ${payload.error}`));
        }
        return structuredResult(payload, { text: lifecycleText(action, model, payload) });
      } catch (error) {
        return errorResult(error);
      }
    },
  );

  server.registerTool(
    "ollama_delete",
    {
      description:
        "DESTRUCTIVE AND IRREVERSIBLE. Use when: a human explicitly names the exact installed tag to delete. Do not use when: you inferred the cleanup yourself, or only want to free memory (ollama_manage unload). Returns: the deleted tag.",
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
}
