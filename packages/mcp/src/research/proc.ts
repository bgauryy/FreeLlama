// Child processes for the research tools: captured stdout/stderr decoded as strict UTF-8 with
// universal newlines, and a per-call timeout. The child stays in the adapter's process group, so the
// MCP layer's group kill on timeout or abort reaches every command the adapter started. A tool
// timeout kills the direct child, then reports `ToolTimeoutError` once it has exited.
import { spawn, spawnSync } from "node:child_process";
import { constants } from "node:os";
import { ToolTimeoutError } from "./errors.js";
import { decodeUtf8, normalizeNewlines } from "./text.js";

export interface ProcessResult {
  stdout: string;
  stderr: string;
  returncode: number;
}

export interface RunOptions {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  timeoutSeconds: number;
}

/** Strict UTF-8 decode, then universal-newline translation. */
export function decodeText(bytes: Uint8Array, source = "command output"): string {
  return normalizeNewlines(decodeUtf8(bytes, source));
}

/** Run `argv` with stdin closed, capturing both output streams. */
export function runProcess(argv: string[], options: RunOptions): Promise<ProcessResult> {
  const timeoutMs = Math.max(options.timeoutSeconds * 1000, 0);
  return new Promise((resolve, reject) => {
    const child = spawn(argv[0]!, argv.slice(1), {
      cwd: options.cwd,
      env: options.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let settled = false;
    const finish = (action: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      action();
    };
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      const fail = () => finish(() => reject(new ToolTimeoutError(
        `command ${JSON.stringify(argv.at(-1))} timed out after ${options.timeoutSeconds} seconds`,
      )));
      if (child.exitCode !== null || child.signalCode !== null) fail();
      else child.once("exit", fail);
    }, timeoutMs);
    child.stdout!.on("data", (chunk: Buffer) => stdout.push(chunk));
    child.stderr!.on("data", (chunk: Buffer) => stderr.push(chunk));
    child.once("error", (error) => finish(() => reject(error)));
    child.once("close", (code, signal) => finish(() => {
      try {
        resolve({
          stdout: decodeText(Buffer.concat(stdout)),
          stderr: decodeText(Buffer.concat(stderr)),
          returncode: code ?? -(signal ? constants.signals[signal] ?? 1 : 1),
        });
      } catch (error) {
        reject(error);
      }
    }));
  });
}

/** Whether `argv` starts and exits 0 within 10 seconds. */
export function probe(argv: string[], env: NodeJS.ProcessEnv): boolean {
  const result = spawnSync(argv[0]!, argv.slice(1), { env, stdio: ["ignore", "pipe", "pipe"], timeout: 10_000, killSignal: "SIGKILL" });
  return !result.error && result.status === 0;
}
