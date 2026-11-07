// Model lifecycle: pull and delete talk to Ollama directly; warm goes through serve's managed
// admission; unload goes through serve when it can (it waits for running tasks), else Ollama.
// `models` covers list/ps/show. `ollama_delete` stays its own tool so the others are not marked
// destructive.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_PULL_TIMEOUT_SECONDS } from "../config.js";
import {
  endpointParam, errorResult, executionPreferenceParam, messagesParam, minPlacementEvidenceParam, ollamaEndpointParam,
  ollamaFetch, parsedResult, structuredResult, systemPromptParam, taskMessages, taskParam,
} from "../helpers.js";
import { progressSink, type ToolExtra } from "../progress.js";
import { serveUnload } from "../serve-http.js";

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

async function ollamaPull(
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

export function registerOllamaLifecycleTools(server: McpServer): void {
  server.registerTool(
    "ollama_manage",
    {
      description:
        "Use when: loading or freeing a model. pull = download one exact tag a human approved with its size; " +
        "warm = load an installed model before a burst of run_task calls; an optional systemPrompt/messages is evaluated into the runner's prompt cache, so later calls that send the same prefix skip that work (they still send it; put it once at the batch top level); " +
        "unload = free a model's memory once its work is done (a model you warmed, or one the user named; waits for its running tasks). " +
        "Do not use when: deleting (ollama_delete) or searching (models). Returns: a receipt with status.",
      inputSchema: z.object({
        action: z.enum(["pull", "warm", "unload"]),
        model: z.string().min(1).describe("exact tag"),
        ollamaEndpoint: ollamaEndpointParam,
        endpoint: endpointParam.describe("serve endpoint (warm, unload)"),
        timeoutSeconds: z.number().int().positive().optional().describe("pull and warm"),
        task: taskParam.removeDefault().optional().describe("warm: the task the model will serve; embedding and decision models load as what they are"),
        contextTokens: z.number().int().positive().optional().describe("warm: num_ctx later calls will use, so they reuse this runner"),
        systemPrompt: systemPromptParam,
        messages: messagesParam,
        keepAlive: z.string().min(1).optional().describe('warm: "-1" pins; omitted = adaptive finite TTL'),
        executionPreference: executionPreferenceParam,
        minPlacementEvidence: minPlacementEvidenceParam,
        priority: z.enum(["interactive", "normal", "background"]).optional(),
        maxWaitSeconds: z.number().int().positive().optional(),
        defer: z.boolean().optional(),
      }).strict(),
      outputSchema: manageResultSchema,
      annotations: { destructiveHint: false },
    },
    async ({ action, model, ollamaEndpoint, endpoint, timeoutSeconds, task, contextTokens, systemPrompt, messages,
      keepAlive, executionPreference, minPlacementEvidence, priority, maxWaitSeconds, defer }, extra) => {
      try {
        const warmFields = { task, contextTokens, systemPrompt, messages, keepAlive, executionPreference,
          minPlacementEvidence, priority, maxWaitSeconds, defer };
        if (action === "pull" && endpoint !== undefined) {
          return errorResult(new Error('pull talks to Ollama; pass `ollamaEndpoint`, not `endpoint`.'));
        }
        const stray = Object.entries(warmFields).filter(([, value]) => value !== undefined).map(([name]) => `\`${name}\``);
        if (action !== "warm" && stray.length > 0) {
          return errorResult(new Error(`${stray.join(", ")} ${stray.length === 1 ? "applies" : "apply"} to action "warm" only.`));
        }
        if (action === "warm") {
          if (ollamaEndpoint !== undefined) return errorResult(new Error('warm runs through serve; pass `endpoint`, not `ollamaEndpoint`.'));
          if (task === "decision") return errorResult(new Error("decision models load on their first question; run the decision task instead of warming."));
          const prefix = taskMessages({ systemPrompt, messages });
          // Loaded on use: the serve client needs the native addon, which pull and unload do not.
          const { warmModelRequest } = await import("../serve-client.js");
          return parsedResult(await warmModelRequest(endpoint, {
            model, task, context_tokens: contextTokens, messages: prefix.length === 0 ? undefined : prefix,
            execution_preference: executionPreference, min_placement_evidence: minPlacementEvidence,
            priority, keep_alive: keepAlive, max_wait_seconds: maxWaitSeconds, timeout_seconds: timeoutSeconds, defer,
          }));
        }
        if (action === "unload" && timeoutSeconds !== undefined) {
          return errorResult(new Error('`timeoutSeconds` is valid only for actions "pull" and "warm".'));
        }
        const data =
          action === "pull"
            ? await ollamaPull(ollamaEndpoint, model, timeoutSeconds, {
                signal: extra.signal,
                onProgress: pullProgressReporter(extra),
              })
            : await unloadModel(model, ollamaEndpoint, extra.signal, serveUnload, endpoint);
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
