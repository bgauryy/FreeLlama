// Ollama lifecycle: talk to Ollama directly (no serve). `models` covers list/ps/show.
// `ollama_delete` stays its own tool so pull/stop are not marked destructive.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { DEFAULT_PULL_TIMEOUT_SECONDS } from "../config.js";
import { clipText, errorResult, ollamaEndpointParam, ollamaFetch, structuredResult } from "../helpers.js";

const manageResultSchema = z.object({
  status: z.string().optional(), progress: z.unknown().optional(),
}).passthrough();
const deleteResultSchema = z.object({
  deleted: z.string(),
}).passthrough();

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
      return { status: "unparsed", raw: clipText(text, 500) };
    }
    return { status: "unparsed", raw: clipText(text, 500) };
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

export function registerOllamaLifecycleTools(server: McpServer): void {
  server.registerTool(
    "ollama_manage",
    {
      description:
        "Use when: a human approved pulling one exact tag (action:pull) or unloading a loaded model (action:stop). " +
        "Do not use when: deleting (ollama_delete), searching (models), or preloading (run_task {warm:true}). Returns: receipt; timeoutSeconds is pull-only.",
      inputSchema: z.object({
        action: z.enum(["pull", "stop"]),
        model: z.string().min(1),
        ollamaEndpoint: ollamaEndpointParam,
        timeoutSeconds: z
          .number()
          .int()
          .positive()
          .optional(),
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
        "DESTRUCTIVE AND IRREVERSIBLE. Use when: a human explicitly names the exact installed tag to delete. Do not use when: you inferred the cleanup yourself, or only want to unload (ollama_manage stop). Returns: the deleted tag.",
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
