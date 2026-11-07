// Managed-serve routes the native addon does not wrap, called over HTTP with the same bearer token.
import { readFile } from "node:fs/promises";
import { DEFAULT_SERVE_ENDPOINT } from "./config.js";
import { ensureServe } from "./serve.js";

/** Bearer header for direct serve calls, from the same token file the native client reads. */
export async function serveAuthHeaders(): Promise<Record<string, string>> {
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
