// Error types the research adapters report as refusals or tool errors instead of crashing.

/** A refused or malformed request: an invalid action, a blocked command or bad configuration. */
export class ValidationError extends Error {
  override name = "ValidationError";
}

/** A Node system error (`ENOENT`, `ECONNREFUSED`, ...): it carries a numeric errno. */
export function isSystemError(error: unknown): error is NodeJS.ErrnoException {
  return error instanceof Error && typeof (error as NodeJS.ErrnoException).errno === "number";
}

/** A tool command ran past its timeout and was killed. */
export class ToolTimeoutError extends Error {
  override name = "ToolTimeoutError";
}

/** A completed subprocess without usable evidence; an empty search is distinct from a failure. */
export class ToolExitError extends Error {
  override name = "ToolExitError";

  constructor(
    readonly exitCode: number,
    readonly output: string,
    readonly status: "empty" | "error" = "error",
  ) {
    super(`${status === "empty" ? "nothing found" : "command failed"} (exit code ${exitCode})` +
      (output ? `\n${output}` : ""));
  }
}

/**
 * Whether a tool runner failure becomes a `tool error: ...` observation for the model instead of
 * ending the run: refusals, non-successful exits, system errors, timeouts, and malformed output.
 */
export function isToolError(error: unknown): error is Error {
  return error instanceof ValidationError || error instanceof ToolTimeoutError || error instanceof ToolExitError || error instanceof SyntaxError
    || isSystemError(error);
}
