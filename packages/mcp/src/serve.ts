// Start `freellama serve` so the shortest path is one tool call.
//
// Every generating tool talks to serve. When the caller uses the default loopback endpoint and
// nothing answers there, start the bundled binary as a child of this server (so it stops with it)
// and wait until it is ready: first until it answers at all, then until `/ready` stops reporting
// that it is still fetching the catalog. The server also starts it once in the background at
// launch, so other local clients of the documented endpoint (skills, scripts, the CLI) find it
// before the agent's first FreeLlama tool call.
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_SERVE_ENDPOINT, FREELLAMA_HOME, REPO_ROOT } from "./config.js";
import type { ProgressSink } from "./progress.js";

const HEALTH_PATH = "/_freellama/v1/health";
const READY_PATH = "/_freellama/v1/ready";
/**
 * Safety ceiling for a child that stays alive without ever answering. A child that exits ends the
 * wait at once, and each caller's own abort signal ends its wait sooner.
 */
const STARTUP_CEILING_MS = 120_000;

let child: ChildProcess | null = null;
let starting: Promise<void> | null = null;
/** Callers waiting on the current start, told each phase it passes. */
const watchers = new Set<ProgressSink>();

/** Serve-backed native calls start the bundled `freellama serve` first when nothing answers. */
export function withServe<Args extends unknown[], R>(
  call: (endpoint: string | null | undefined, ...args: Args) => Promise<R>,
): (endpoint: string | null | undefined, ...args: Args) => Promise<R> {
  return async (endpoint, ...args) => {
    await ensureServe(endpoint ?? undefined);
    return call(endpoint, ...args);
  };
}

function autostartEnabled(): boolean {
  return process.env.FREELLAMA_MCP_AUTOSTART_SERVE !== "0";
}

function loopbackListen(endpoint: string): string | null {
  try {
    const url = new URL(endpoint);
    if (!["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)) return null;
    return `${url.hostname === "localhost" ? "127.0.0.1" : url.hostname}:${url.port || "80"}`;
  } catch {
    return null;
  }
}

type Readiness = { state: "absent" } | { state: "ready" } | { state: "waiting"; waitingFor: string; detail?: string };

/**
 * Whether a server owns the port, and if so whether it is ready. A server without `/ready` (older
 * releases answer 404) or one that refuses the probe (401 behind a token) counts as ready once it
 * answers: the tool's own call then reports any real problem.
 */
export async function readiness(endpoint: string, timeoutMs: number): Promise<Readiness> {
  let response: Response;
  try {
    response = await fetch(`${endpoint.replace(/\/$/, "")}${READY_PATH}`, { signal: AbortSignal.timeout(timeoutMs) });
  } catch {
    return { state: "absent" };
  }
  const body = await response.json().catch(() => undefined) as { ready?: unknown; waiting_for?: unknown; detail?: unknown } | undefined;
  if (response.status !== 503 && body?.ready !== false) return { state: "ready" };
  return {
    state: "waiting",
    waitingFor: typeof body?.waiting_for === "string" ? body.waiting_for : "startup",
    ...(typeof body?.detail === "string" ? { detail: body.detail } : {}),
  };
}

function announce(message: string): void {
  for (const watcher of watchers) watcher(message);
}

/** `--runtime-config` for the operator's `runtime.toml` in the FreeLlama home, when it exists. */
function runtimeConfigArgs(): string[] {
  if (process.env.FREELLAMA_RUNTIME_CONFIG) return [];
  const file = path.join(FREELLAMA_HOME, "runtime.toml");
  return existsSync(file) ? ["--runtime-config", file] : [];
}

async function answers(endpoint: string, timeoutMs: number): Promise<boolean> {
  try {
    // Any HTTP response (including 401 behind an auth token) means a server owns the port.
    await fetch(`${endpoint.replace(/\/$/, "")}${HEALTH_PATH}`, { signal: AbortSignal.timeout(timeoutMs) });
    return true;
  } catch {
    return false;
  }
}

function serveBinaryCandidates(): string[] {
  const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
  const candidates = [
    path.join(REPO_ROOT, "target", "release", exe),
    path.join(REPO_ROOT, "target", "debug", exe),
  ];
  const requireFromHere = createRequire(fileURLToPath(import.meta.url));
  const abiSuffix = ({ linux: "-gnu", win32: "-msvc" } as Record<string, string>)[process.platform] ?? "";
  try {
    const manifest = requireFromHere.resolve(`@octocodeai/freellama-native-${process.platform}-${process.arch}${abiSuffix}/package.json`);
    candidates.push(path.join(path.dirname(manifest), exe));
  } catch {
    // Optional platform packages for other targets are absent by design.
  }
  if (process.env.FREELLAMA_SERVE_BINARY) candidates.unshift(process.env.FREELLAMA_SERVE_BINARY);
  return candidates;
}

/** Background start at launch: a failure is logged and the next tool call retries and reports it. */
export function startServeInBackground(): void {
  ensureServe(undefined).catch((error: unknown) => {
    process.stderr.write(`[freellama serve] background start failed: ${error instanceof Error ? error.message : String(error)}\n`);
  });
}

export function stopAutostartedServe(): void {
  if (child && child.exitCode === null) child.kill("SIGTERM");
  child = null;
}

/** Wait for `work`, or reject when the caller's own request is cancelled first. */
function untilAborted(work: Promise<void>, signal: AbortSignal | undefined): Promise<void> {
  if (!signal) return work;
  if (signal.aborted) return Promise.reject(new Error("Cancelled while freellama serve was starting."));
  return new Promise((resolve, reject) => {
    const abort = () => reject(new Error("Cancelled while freellama serve was starting; it keeps starting for the next call."));
    signal.addEventListener("abort", abort, { once: true });
    work.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

/**
 * Make sure serve answers at `endpoint`. A no-op for explicit non-default or remote endpoints,
 * when FREELLAMA_MCP_AUTOSTART_SERVE=0, or when something already owns the port. After starting
 * the bundled binary it waits while the child is alive: until it answers, then while `/ready`
 * says the catalog is still loading. A `/ready` that waits for Ollama or a CPU backend ends the
 * wait, since the tool's call then reports what is down. `report` receives each phase; `signal` ends this
 * caller's wait without stopping the start. Startup failures reject the tool request while
 * leaving the MCP transport available for a retry.
 */
export async function ensureServe(
  endpoint: string | undefined,
  options: { signal?: AbortSignal; report?: ProgressSink } = {},
): Promise<void> {
  const target = endpoint ?? DEFAULT_SERVE_ENDPOINT;
  if (!autostartEnabled() || target !== DEFAULT_SERVE_ENDPOINT) return;
  const listen = loopbackListen(target);
  if (!listen) return;
  if (!starting && child && child.exitCode === null && (await answers(target, 1_000))) return;
  if (options.report) watchers.add(options.report);
  try {
    await untilAborted(startServe(target, listen), options.signal);
  } finally {
    if (options.report) watchers.delete(options.report);
  }
}

const STDERR_TAIL_BYTES = 4096;
const STDERR_TAIL_LINES = 20;

function startServe(target: string, listen: string): Promise<void> {
  starting ??= (async () => {
    try {
      if (await answers(target, 1_000)) return;
      const binary = serveBinaryCandidates().find((candidate) => existsSync(candidate));
      if (!binary) return;
      announce("starting freellama serve");
      // stdout is the MCP transport: the child must never write to it.
      const spawned = spawn(binary, ["serve", "--listen", listen, "--upstream", DEFAULT_OLLAMA_ENDPOINT, ...runtimeConfigArgs()], {
        stdio: ["ignore", "ignore", "pipe"],
      });
      child = spawned;
      // The agent never sees the child's stderr, so keep its tail for the exit error.
      let stderrTail = "";
      const closed = new Promise<void>((resolve) => spawned.once("close", () => resolve()));
      spawned.stderr?.on("data", (chunk: Buffer) => {
        stderrTail = (stderrTail + chunk).slice(-STDERR_TAIL_BYTES);
        process.stderr.write(`[freellama serve] ${chunk}`);
        // The child's startup phases, one line at a time, for callers watching the start.
        const line = String(chunk).trim().split("\n").at(-1)?.trim();
        if (line && starting) announce(`freellama serve: ${line}`);
      });
      spawned.once("exit", () => {
        if (child === spawned) child = null;
      });
      await new Promise<void>((resolve, reject) => {
        spawned.once("spawn", resolve);
        spawned.once("error", (error) => {
          if (child === spawned) child = null;
          reject(new Error(`Could not start freellama serve at ${target}: ${error.message}`));
        });
      });
      const ceiling = Date.now() + STARTUP_CEILING_MS;
      let last = "";
      while (Date.now() < ceiling && child === spawned) {
        const state = await readiness(target, 1_000);
        if (state.state === "ready") {
          announce("freellama serve ready");
          return;
        }
        if (state.state === "waiting") {
          const phase = `freellama serve: waiting for ${state.waitingFor}${state.detail ? ` (${state.detail})` : ""}`;
          if (phase !== last) announce(phase);
          last = phase;
          // It answers. Only a catalog still loading resolves itself; an unreachable Ollama or
          // CPU backend will not appear by waiting here, and the tool's call reports it.
          if (state.waitingFor !== "catalog" && state.waitingFor !== "startup") return;
        }
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
      if (child === spawned) {
        stopAutostartedServe();
        throw new Error(`freellama serve stayed alive but did not become ready at ${target} within ${STARTUP_CEILING_MS}ms; it was stopped.`);
      }
      // Let the pipe drain so the final lines (usually the cause) are included.
      await Promise.race([closed, new Promise((resolve) => setTimeout(resolve, 500))]);
      const lines = stderrTail.trim().split("\n").slice(-STDERR_TAIL_LINES).join("\n");
      throw new Error(
        `freellama serve exited before becoming ready at ${target} (exit ${spawned.exitCode}, signal ${spawned.signalCode}).` +
          (lines ? ` Last stderr:\n${lines}` : ""),
      );
    } finally {
      starting = null;
    }
  })();
  return starting;
}
