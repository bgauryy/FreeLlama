import { z } from "zod";
import {
  configSchema,
  healthSchema,
  inventorySchema,
  machineSchema,
  statusSchema,
  usageSchema,
} from "../shared/contracts";
import type { Snapshot, Source } from "../shared/contracts";

interface CollectorOptions {
  endpoint: string;
  token?: string;
  fetcher?: (url: string, init: RequestInit) => Promise<Response>;
  now?: () => number;
  timeoutMs?: number;
}

const MAX_BODY_BYTES = 8 * 1024 * 1024;

async function readJson(response: Response): Promise<unknown> {
  if (!response.body) throw new Error("empty response");
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_BODY_BYTES) throw new Error("response exceeds limit");
      chunks.push(value);
    }
    return JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
  } finally {
    await reader.cancel().catch(() => {});
  }
}

// One cache per source, shared across browser tabs. There is no background inference or polling:
// a browser read refreshes expired entries and concurrent reads join the same upstream promise.
export function createCollector(options: CollectorOptions) {
  const fetcher = options.fetcher ?? fetch;
  const now = options.now ?? Date.now;
  function source<T>(path: string, schema: z.ZodType<T>, ttl: number) {
    let cached: Source<T> = {
      state: "unavailable",
      data: null,
      updated_at: null,
      error: "No observation yet.",
    };
    let nextRead = 0;
    let pending: Promise<Source<T>> | undefined;
    async function refresh(): Promise<Source<T>> {
      let error: string | undefined;
      try {
        const response = await fetcher(
          `${options.endpoint}/_freellama/v1/${path}`,
          {
            method: "GET",
            redirect: "error",
            cache: "no-store",
            headers: {
              ...(options.token
                ? { Authorization: `Bearer ${options.token}` }
                : {}),
              Accept: "application/json",
            },
            signal: AbortSignal.timeout(options.timeoutMs ?? 8000),
          },
        );
        if (!response.ok) {
          await response.body?.cancel();
          error =
            response.status === 401 || response.status === 403
              ? `HTTP ${response.status}: check the server's FREELLAMA_AUTH_TOKEN_FILE token file.`
              : `Control API returned HTTP ${response.status}.`;
        } else {
          const parsed = schema.safeParse(await readJson(response));
          if (!parsed.success)
            error =
              "Response does not match the expected control API schema; rebuild or update FreeLlama.";
          else
            cached = {
              state: "live",
              data: parsed.data,
              updated_at: new Date(now()).toISOString(),
              error: null,
            };
        }
      } catch (failure) {
        // Do not relay upstream bodies, headers, or exception text: they may contain secrets.
        error =
          failure instanceof Error &&
          (failure.name === "TimeoutError" || failure.name === "AbortError")
            ? "Control API request timed out."
            : "Cannot read the control API. Check that freellama serve is running and the endpoint is correct.";
      }
      if (error)
        cached =
          cached.state === "unavailable"
            ? { state: "unavailable", data: null, updated_at: null, error }
            : { ...cached, state: "stale", error };
      nextRead = now() + ttl;
      return cached;
    }
    return () => {
      if (pending) return pending;
      if (now() < nextRead) return Promise.resolve(cached);
      pending = refresh().finally(() => {
        pending = undefined;
      });
      return pending;
    };
  }
  const read = {
    status: source("status", statusSchema, 2000),
    health: source("health", healthSchema, 5000),
    machine: source("machine", machineSchema, 60_000),
    models: source("models", inventorySchema, 30_000),
    usage: source("usage?days=7", usageSchema, 10_000),
    config: source("config", configSchema, 10_000),
  };
  return {
    async snapshot(): Promise<Snapshot> {
      const [status, health, machine, models, usage, config] =
        await Promise.all([
          read.status(),
          read.health(),
          read.machine(),
          read.models(),
          read.usage(),
          read.config(),
        ]);
      return {
        schema_version: 1,
        collected_at: new Date(now()).toISOString(),
        endpoint: options.endpoint,
        poll_interval_ms: 2000,
        sources: { status, health, machine, models, usage, config },
      };
    },
  };
}
