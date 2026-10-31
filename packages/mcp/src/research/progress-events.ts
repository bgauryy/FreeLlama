// Research progress: the structured events the adapter loop emits as it works, and the stderr line
// protocol that carries them to the MCP server. The server turns them into MCP progress messages
// (src/research-progress.ts); the adapter emits nothing unless the server asked for progress.
import type { Action } from "./actions.js";
import type { Env } from "./context-policy.js";

/** Set to "stderr" by the MCP server when its client asked for progress. */
export const PROGRESS_ENV = "FREELLAMA_AGENT_PROGRESS";
/**
 * Prefix of one progress line on the adapter's stderr. Stderr already carries only diagnostics
 * (the answer travels in result.json), and the adapter's tool subprocesses write to their own
 * captured pipes, so no model-run command can write a line here.
 */
export const PROGRESS_PREFIX = "@freellama-progress ";

/** `turn` counts model calls from 1, as the loop's `maxTurns` budget does. */
export type ResearchProgressEvent =
  | { event: "model"; turn: number; maxTurns: number }
  | { event: "retry"; attempt: number; attempts: number }
  | { event: "repair"; turn: number; maxTurns: number }
  | { event: "action"; turn: number; maxTurns: number; action: Action; repeat?: boolean }
  | { event: "finish"; turn: number; maxTurns: number }
  | { event: "finalize"; maxTurns: number };

export type ProgressEmitter = (event: ResearchProgressEvent) => void;

/**
 * The emitter for this run: one JSON line per event on stderr when `env` asks for it, else
 * `undefined`. A failed write never fails the run; progress is advisory.
 */
export function progressEmitter(
  env: Env,
  write: (line: string) => void = (line) => {
    process.stderr.write(line);
  },
): ProgressEmitter | undefined {
  if (env[PROGRESS_ENV] !== "stderr") return undefined;
  return (event) => {
    try {
      write(`${PROGRESS_PREFIX}${JSON.stringify(event)}\n`);
    } catch {
      // The server has gone or the pipe is closed; the result file still carries the run.
    }
  };
}
