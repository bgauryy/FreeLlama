// Managed-serve routes the native addon does not wrap, called over HTTP with the same bearer token.
import { readFile } from "node:fs/promises";
import { DEFAULT_SERVE_ENDPOINT } from "./config.js";
import { ensureServe } from "./serve.js";

/** Bearer header for direct serve calls, from the same token file the native client reads. */
async function serveAuthHeaders(): Promise<Record<string, string>> {
  const tokenFile = process.env.FREELLAMA_AUTH_TOKEN_FILE?.trim();
  if (!tokenFile) return {};
  const token = (await readFile(tokenFile, "utf8")).trim();
  return { authorization: `Bearer ${token}` };
}

/**
 * Unload `model` through serve, which waits for that model's running tasks and records the
 * unload. Returns `undefined` when serve is unreachable or predates `/unload` (HTTP 404), so the
 * caller can ask Ollama directly; any other refusal (such as `unload_wait_exceeded`) is thrown
 * with serve's receipt as the message.
 */
export async function serveUnload(
  model: string,
  options: { endpoint?: string; maxWaitSeconds?: number; signal?: AbortSignal } = {},
): Promise<Record<string, unknown> | undefined> {
  const endpoint = (options.endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "");
  try {
    await ensureServe(options.endpoint, { signal: options.signal });
  } catch {
    return undefined;
  }
  let response: Response;
  try {
    response = await fetch(`${endpoint}/_freellama/v1/unload`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(await serveAuthHeaders()) },
      body: JSON.stringify({ model, ...(options.maxWaitSeconds !== undefined ? { max_wait_seconds: options.maxWaitSeconds } : {}) }),
      signal: options.signal,
    });
  } catch (error) {
    if (options.signal?.aborted) throw error;
    return undefined;
  }
  if (response.status === 404) return undefined;
  const text = await response.text();
  if (!response.ok) throw new Error(text || `freellama serve unload -> HTTP ${response.status}`);
  return JSON.parse(text) as Record<string, unknown>;
}

/** Running-batch progress as `GET /task-batches/{id}` reports it. */
export type BatchProgressView = {
  total: number;
  done: number;
  succeeded: number;
  failed: number;
  running: string[];
  queued: number;
  finished: boolean;
};

/**
 * Read a batch's progress. `undefined` when it is not (yet) known; `"unsupported"` when the
 * server predates batch progress (the route is missing, so its 404 is not serve's JSON error).
 */
export async function serveBatchProgress(
  id: string,
  options: { endpoint?: string; signal?: AbortSignal } = {},
): Promise<BatchProgressView | "unsupported" | undefined> {
  const endpoint = (options.endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "");
  let response: Response;
  try {
    response = await fetch(`${endpoint}/_freellama/v1/task-batches/${encodeURIComponent(id)}`, {
      headers: await serveAuthHeaders(),
      signal: options.signal,
    });
  } catch {
    return undefined;
  }
  const text = await response.text();
  if (response.ok) return JSON.parse(text) as BatchProgressView;
  if (response.status === 404) {
    // serve's own 404 for an unknown id is JSON naming progress_id; anything else is an older server.
    return text.includes("progress_id") ? undefined : "unsupported";
  }
  return undefined;
}
