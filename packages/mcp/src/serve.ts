// Start `freellama serve` on demand so the shortest path is one tool call.
//
// Every generating tool talks to serve, and the MCP server used to assume someone had started it
// in another terminal; the first run_task on a fresh install failed with a connection error. When
// the caller uses the default loopback endpoint and nothing answers there, start the bundled
// binary as a child of this server (so it stops with it) and wait for its health route.
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { DEFAULT_OLLAMA_ENDPOINT, DEFAULT_SERVE_ENDPOINT, REPO_ROOT } from "./config.js";

const HEALTH_PATH = "/_freellama/v1/health";
const STARTUP_TIMEOUT_MS = 15_000;

let child: ChildProcess | null = null;
let starting: Promise<void> | null = null;

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

async function answers(endpoint: string, timeoutMs: number): Promise<boolean> {
  try {
    // Any HTTP response (including 401 behind an auth token) means a server owns the port.
    await fetch(`${endpoint.replace(/\/$/, "")}${HEALTH_PATH}`, { signal: AbortSignal.timeout(timeoutMs) });
    return true;
  } catch {
    return false;
  }
}

export function serveBinaryCandidates(): string[] {
  const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
  const candidates = [
    path.join(REPO_ROOT, "target", "release", exe),
    path.join(REPO_ROOT, "target", "debug", exe),
  ];
  const requireFromHere = createRequire(fileURLToPath(import.meta.url));
  const suffixes = process.platform === "linux" ? ["-gnu", "-musl"] : process.platform === "win32" ? ["-msvc"] : [""];
  for (const suffix of suffixes) {
    try {
      const manifest = requireFromHere.resolve(`@octocodeai/freellama-native-${process.platform}-${process.arch}${suffix}/package.json`);
      candidates.push(path.join(path.dirname(manifest), exe));
    } catch {
      // Optional platform packages for other targets are absent by design.
    }
  }
  if (process.env.FREELLAMA_SERVE_BINARY) candidates.unshift(process.env.FREELLAMA_SERVE_BINARY);
  return candidates;
}

export function stopAutostartedServe(): void {
  if (child && child.exitCode === null) child.kill("SIGTERM");
  child = null;
}

/**
 * Make sure serve answers at `endpoint`. A no-op for explicit non-default or remote endpoints,
 * when FREELLAMA_MCP_AUTOSTART_SERVE=0, or when something already answers. Startup failures
 * reject the tool request while leaving the MCP transport available for a retry.
 */
export async function ensureServe(endpoint: string | undefined): Promise<void> {
  const target = endpoint ?? DEFAULT_SERVE_ENDPOINT;
  if (!autostartEnabled() || target !== DEFAULT_SERVE_ENDPOINT) return;
  const listen = loopbackListen(target);
  if (!listen) return;
  if (child && child.exitCode === null && (await answers(target, 1_000))) return;
  starting ??= (async () => {
    try {
      if (await answers(target, 1_000)) return;
      const binary = serveBinaryCandidates().find((candidate) => existsSync(candidate));
      if (!binary) return;
      // stdout is the MCP transport: the child must never write to it.
      const spawned = spawn(binary, ["serve", "--listen", listen, "--upstream", DEFAULT_OLLAMA_ENDPOINT], {
        stdio: ["ignore", "ignore", "pipe"],
      });
      child = spawned;
      spawned.stderr?.on("data", (chunk: Buffer) => process.stderr.write(`[freellama serve] ${chunk}`));
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
      const deadline = Date.now() + STARTUP_TIMEOUT_MS;
      while (Date.now() < deadline && child === spawned) {
        if (await answers(target, 500)) return;
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
      if (child === spawned) {
        stopAutostartedServe();
        throw new Error(`freellama serve did not become ready at ${target} within ${STARTUP_TIMEOUT_MS}ms.`);
      }
      throw new Error(`freellama serve exited before becoming ready at ${target} (exit ${spawned.exitCode}, signal ${spawned.signalCode}).`);
    } finally {
      starting = null;
    }
  })();
  await starting;
}
