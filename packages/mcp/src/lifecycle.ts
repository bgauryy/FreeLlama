// Process lifecycle: delegate subprocess tracking and shutdown cleanup.
import { type ChildProcess, spawn } from "node:child_process";
import { stopAutostartedServe } from "./serve.js";

// `delegate_research` spawns a Node adapter subprocess that can run for minutes. If this server goes
// away first — client disconnect, Ctrl-C, a supervisor restart — an untracked child keeps a local
// model pinned in VRAM with nothing left to return its answer to. Track every live child and take
// it down with the server.
export const liveDelegates = new Set<ChildProcess>();

// On POSIX a delegate is spawned detached, so it leads its own process group and the adapter's
// grandchildren (`npx octocode`, sandbox shells) can be killed with it. Windows has no process
// groups for this, so there the child alone is killed.
const DETACH_DELEGATES = process.platform !== "win32";

/** SIGKILL a delegate's process group (POSIX) or the child itself; already-gone is fine. */
export function killDelegateTree(child: ChildProcess): void {
  if (DETACH_DELEGATES && child.pid !== undefined) {
    try {
      process.kill(-child.pid, "SIGKILL");
      return;
    } catch {
      // No such group left; fall through to the child.
    }
  }
  try {
    child.kill("SIGKILL");
  } catch {
    // Already gone; nothing to clean up.
  }
}

type DelegateRun = { child: ChildProcess; done: Promise<{ stdout: string; stderr: string }> };

/**
 * `execFile` with the delegate's group semantics. execFile ignores `detached` and its timeout,
 * maxBuffer, and abort paths kill only the direct child, so this mirrors its contract (utf8
 * capture, reject on non-zero exit/signal with `Command failed: ...`, maxBuffer RangeError,
 * AbortError) while every kill takes down the whole process group.
 */
export function runDelegate(
  file: string,
  args: string[],
  options: { env: NodeJS.ProcessEnv; signal?: AbortSignal; timeoutMs: number; maxBuffer: number },
): DelegateRun {
  const child = spawn(file, args, { env: options.env, detached: DETACH_DELEGATES, stdio: ["ignore", "pipe", "pipe"] });
  const done = new Promise<{ stdout: string; stderr: string }>((resolve, reject) => {
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    let outLen = 0;
    let errLen = 0;
    let failure: Error | undefined;
    let timedOut = false;
    let settled = false;
    const fail = (error: Error) => {
      if (failure === undefined) failure = error;
      killDelegateTree(child);
    };
    const timer = setTimeout(() => { timedOut = true; killDelegateTree(child); }, options.timeoutMs);
    const onAbort = () => {
      const abort = new Error("The operation was aborted", { cause: options.signal?.reason });
      abort.name = "AbortError";
      (abort as NodeJS.ErrnoException).code = "ABORT_ERR";
      fail(abort);
    };
    if (options.signal?.aborted) onAbort();
    else options.signal?.addEventListener("abort", onAbort, { once: true });
    const collect = (chunks: Buffer[], name: "stdout" | "stderr") => (chunk: Buffer) => {
      const length = (name === "stdout" ? (outLen += chunk.length) : (errLen += chunk.length));
      if (length > options.maxBuffer) {
        const overflow = new RangeError(`${name} maxBuffer length exceeded`);
        (overflow as NodeJS.ErrnoException).code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        fail(overflow);
        return;
      }
      chunks.push(chunk);
    };
    child.stdout!.on("data", collect(out, "stdout"));
    child.stderr!.on("data", collect(err, "stderr"));
    const finish = (code: number | null, signal: NodeJS.Signals | null, spawnError?: Error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
      const stdout = Buffer.concat(out).toString("utf8");
      const stderr = Buffer.concat(err).toString("utf8");
      if (spawnError ?? failure) { reject(spawnError ?? failure); return; }
      if (code === 0 && signal === null) { resolve({ stdout, stderr }); return; }
      const error = new Error(`Command failed: ${[file, ...args].join(" ")}\n${stderr}`) as Error & {
        code: number | null; killed: boolean; signal: NodeJS.Signals | null; stdout: string; stderr: string;
      };
      Object.assign(error, { code, killed: timedOut, signal, stdout, stderr });
      reject(error);
    };
    child.once("error", (error) => finish(null, null, error));
    child.once("close", (code, signal) => finish(code, signal));
  });
  return { child, done };
}

function killLiveDelegates(): void {
  for (const child of liveDelegates) killDelegateTree(child);
  liveDelegates.clear();
}

/** Install exit, stdin-close, signal, and EPIPE handlers. Call once from the server entry point. */
export function installProcessLifecycle(): void {
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
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
    process.on(signal, () => {
      killLiveDelegates();
      process.exit(0);
    });
  }
}
