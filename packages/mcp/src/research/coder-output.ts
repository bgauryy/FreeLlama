// Raw command audit, separate from the bounded model preview. Local synchronous writes apply
// backpressure one pipe chunk at a time; no queued-output growth or silent middle-byte eviction.
import { createHash, randomUUID } from "node:crypto";
import { closeSync, openSync, writeSync, writeFileSync, renameSync, unlinkSync, existsSync } from "node:fs";

export const DEFAULT_COMMAND_OUTPUT_BYTES = 16 * 1024 * 1024;
export const MAX_COMMAND_OUTPUT_BYTES = 256 * 1024 * 1024;
export type OutputEnd = "complete" | "output_limit" | "write_error" | "timeout" | "pipes_open_after_exit" | "spawn_error";
export interface CommandOutputAudit {
  path: string;
  metadata_path: string;
  bytes: number;
  sha256: string;
  limit_bytes: number;
  complete: boolean;
  end_reason: OutputEnd;
  ordering: "observed stdout/stderr chunk arrival; not a perfect cross-stream interleaving";
  error?: string;
}

type Write = (fd: number, data: Buffer, offset: number, length: number) => number;
export class CommandOutput {
  private readonly fd: number;
  private readonly hash = createHash("sha256");
  private bytes = 0;
  private reason: OutputEnd = "complete";
  private error?: string;
  private closed = false;
  private readonly started = new Date().toISOString();

  constructor(readonly path: string, readonly limit = DEFAULT_COMMAND_OUTPUT_BYTES, private readonly write: Write = writeSync, private readonly command = "") {
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_COMMAND_OUTPUT_BYTES) throw new Error("invalid command output byte limit");
    this.fd = openSync(path, "wx", 0o600);
    try { this.metadata({ state: "incomplete", reason: "command running or interrupted before finalization" }); }
    catch (error) { closeSync(this.fd); throw error; }
  }

  private metadata(fields: Record<string, unknown>): void {
    const file = `${this.path}.json`, temporary = `${file}.${randomUUID()}.tmp`;
    try {
      writeFileSync(temporary, `${JSON.stringify({ command: this.command, started_at: this.started, ...fields })}\n`, { flag: "wx", mode: 0o600 });
      renameSync(temporary, file);
    } finally { if (existsSync(temporary)) unlinkSync(temporary); }
  }

  get size(): number { return this.bytes; }

  /** Retain the exact prefix up to the cap; false tells the owner to terminate the producer. */
  append(chunk: Buffer): boolean {
    if (this.closed || this.reason !== "complete") return false;
    const length = Math.min(chunk.length, this.limit - this.bytes);
    let offset = 0;
    try {
      while (offset < length) {
        const count = this.write(this.fd, chunk, offset, length - offset);
        if (!Number.isInteger(count) || count < 1 || count > length - offset) throw new Error("audit write made no valid progress");
        this.hash.update(chunk.subarray(offset, offset + count));
        this.bytes += count;
        offset += count;
      }
    } catch (error) {
      this.reason = "write_error";
      this.error = error instanceof Error ? error.message : String(error);
      return false;
    }
    if (length < chunk.length) { this.reason = "output_limit"; return false; }
    return true;
  }

  finish(reason: OutputEnd = "complete", error?: string, execution: Record<string, unknown> = {}): CommandOutputAudit {
    if (this.closed) throw new Error("command output audit is already closed");
    this.closed = true;
    if (this.reason === "complete") this.reason = reason;
    if (error) this.error ??= error;
    try { closeSync(this.fd); }
    catch (cause) { this.reason = "write_error"; this.error = cause instanceof Error ? cause.message : String(cause); }
    const audit: CommandOutputAudit = { path: this.path, metadata_path: `${this.path}.json`, bytes: this.bytes, sha256: this.hash.digest("hex"), limit_bytes: this.limit,
      complete: this.reason === "complete", end_reason: this.reason,
      ordering: "observed stdout/stderr chunk arrival; not a perfect cross-stream interleaving", ...(this.error ? { error: this.error } : {}) };
    try { this.metadata({ state: audit.complete ? "complete" : "incomplete", output: audit, execution }); }
    catch (cause) { audit.complete = false; audit.end_reason = "write_error"; audit.error = cause instanceof Error ? cause.message : String(cause); }
    return audit;
  }
}
