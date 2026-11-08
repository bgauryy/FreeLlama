// Coder adapter: a local Ollama model that researches, edits files and runs the caller's allowed
// commands in an isolated copy of the workspace (./isolated-copy.ts), never in the user's tree.
// Same loop as the research adapters (agent-loop.ts): repeat suppression, cited-path grounding
// and forced finalization; the finish reports work done, so it is not asked for a "Source:".
// The result adds `coder`: the agent's diff against HEAD plus the caller's uncommitted changes,
// the changed files and the last run outputs. The caller reviews it and applies it with `git apply`.
//
// Run as `node coder-agent.js` with the research adapter environment plus
// FREELLAMA_MCP_ALLOW_CODER=1 (operator opt-in), FREELLAMA_CODER_COMMANDS (a JSON array of exact
// allowed commands) and optionally FREELLAMA_CODER_COPY_DIR (an empty directory for the copy),
// FREELLAMA_CODER_PATCH (where to write the patch), FREELLAMA_CODER_RUN_TIMEOUT_SECONDS,
// FREELLAMA_CODER_MAX_COPY_BYTES, FREELLAMA_CODER_WRITABLE (extra directories allowed commands may
// write, such as a cargo home) and FREELLAMA_CODER_UNSANDBOXED_RUN=1 (run without an OS sandbox).
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parseJsonAction } from "./actions.js";
import { type Action, type AdapterSession, type AdapterSpec, type ToolCall, isPositiveInt } from "./adapter.js";
import { runAdapter } from "./agent-loop.js";
import {
  CommandRunner, DEFAULT_RUN_TIMEOUT_SECONDS, MAX_RUN_TIMEOUT_SECONDS, WRITE_MAX_BYTES, allowedCommand, applyEdit, describeRun,
  normalizeAllowedCommands, runEnvironment, secretPaths, writeWholeFile, type EditAction, type RunOutcome, type WriteAction,
} from "./coder-actions.js";
import { writeFailureResult } from "./context-manager.js";
import type { Env } from "./context-policy.js";
import { ValidationError } from "./errors.js";
import { DEFAULT_MAX_COPY_BYTES, agentChanges, createIsolatedCopy, newCopyContainer, removeIsolatedCopy, type IsolatedCopy } from "./isolated-copy.js";
import { CODER_SYSTEM_PROMPT } from "./prompts.js";
import { ReadOnlyShell } from "./sandbox.js";
import { forgetWorkspaceIndex, validateRead, validateSearch } from "./structured-actions.js";
import { structuredCall } from "./structured-call.js";
import { trimSpace } from "./text.js";
import { workspaceOverview } from "./workspace-overview.js";
import { escapesRoot } from "./workspace-path.js";
import { isEntry } from "./entry.js";

export const OPT_IN_ENV = "FREELLAMA_MCP_ALLOW_CODER";
const ACTIONS = ["search", "read", "list", "shell", "edit", "write", "run", "page", "finish"];
/** Runs whose output the result keeps (the newest). */
const KEPT_RUN_OUTPUTS = 3;

const requireString = (value: Record<string, unknown>, key: string, action: string, nonEmpty = true) => {
  const field = value[key];
  if (typeof field !== "string" || (nonEmpty && !field)) {
    throw new ValidationError(`${action} action requires a ${nonEmpty ? "non-empty " : ""}string ${key}`);
  }
};

export function parseAction(content: string): Action {
  const value = parseJsonAction(content);
  const action = String(value.action);
  if (!ACTIONS.includes(action)) throw new ValidationError(`unsupported action for coder adapter: ${action} (use ${ACTIONS.join(", ")})`);
  if (action === "shell" && (typeof value.command !== "string" || !trimSpace(value.command))) {
    throw new ValidationError("shell action requires a non-empty string command");
  }
  if (action === "run") requireString(value, "command", "run");
  if (action === "page" && (!isPositiveInt(value.step) || !isPositiveInt(value.page))) {
    throw new ValidationError("page action requires positive integer step and page");
  }
  if (action === "finish") requireString(value, "answer", "finish", false);
  if (action === "search") validateSearch(value);
  if (action === "read") validateRead(value);
  if (action === "list" && value.path !== undefined && typeof value.path !== "string") throw new ValidationError("list path must be a string");
  if (action === "edit") {
    requireString(value, "path", "edit");
    requireString(value, "search", "edit");
    requireString(value, "replace", "edit", false);
    if (value.search === value.replace) throw new ValidationError("edit search and replace are identical; nothing would change");
  }
  if (action === "write") {
    requireString(value, "path", "write");
    requireString(value, "content", "write", false);
  }
  return value;
}

export interface CoderOptions {
  /** Exact commands `run` may execute. */
  commands: string[];
  /** An empty directory to hold the copy; default: a new one under the system temp directory. */
  container?: string;
  /** Where the raw patch is written, when set. */
  patchPath?: string;
  runTimeoutSeconds?: number;
  maxCopyBytes?: number;
  /** Directories outside the copy that allowed commands may also write (operator-named). */
  writable?: string[];
  allowUnsandboxedRun?: boolean;
  osSandbox?: "auto" | "off";
  /** Link the workspace's node_modules into the copy (default true). */
  linkDependencies?: boolean;
}

function positiveNumber(raw: string | undefined, fallback: number, max: number): number {
  const value = raw === undefined || raw === "" ? Number.NaN : Number(raw);
  return Number.isFinite(value) && value > 0 ? Math.min(value, max) : fallback;
}

/** Options from the adapter environment the MCP server sets. */
export function coderOptionsFromEnv(env: Env): CoderOptions {
  let commands: unknown;
  try {
    commands = JSON.parse(env.FREELLAMA_CODER_COMMANDS ?? "[]");
  } catch {
    throw new ValidationError("FREELLAMA_CODER_COMMANDS must be a JSON array of command strings");
  }
  return {
    commands: normalizeAllowedCommands(commands),
    container: env.FREELLAMA_CODER_COPY_DIR || undefined,
    patchPath: env.FREELLAMA_CODER_PATCH || undefined,
    runTimeoutSeconds: positiveNumber(env.FREELLAMA_CODER_RUN_TIMEOUT_SECONDS, DEFAULT_RUN_TIMEOUT_SECONDS, MAX_RUN_TIMEOUT_SECONDS),
    maxCopyBytes: positiveNumber(env.FREELLAMA_CODER_MAX_COPY_BYTES, DEFAULT_MAX_COPY_BYTES, Number.MAX_SAFE_INTEGER),
    writable: (env.FREELLAMA_CODER_WRITABLE ?? "").split(path.delimiter).filter(Boolean),
    allowUnsandboxedRun: env.FREELLAMA_CODER_UNSANDBOXED_RUN === "1",
    osSandbox: env.FREELLAMA_AGENT_OS_SANDBOX === "off" ? "off" : "auto",
  };
}

/** Operator-named writable directories, by real path; none may hold, or sit inside, the workspace. */
function writableDirs(dirs: string[], workspace: string): string[] {
  return dirs.map((dir) => {
    if (!path.isAbsolute(dir)) throw new ValidationError(`FREELLAMA_CODER_WRITABLE entry ${dir} must be absolute`);
    const real = existsSync(dir) ? realpathSync(dir) : path.resolve(dir);
    if (!escapesRoot(workspace, real) || !escapesRoot(real, workspace)) {
      throw new ValidationError(`FREELLAMA_CODER_WRITABLE entry ${dir} overlaps the workspace; the coder never writes the user's files`);
    }
    return real;
  });
}

const sha1 = (file: string) => {
  try {
    return createHash("sha1").update(readFileSync(file)).digest("hex");
  } catch {
    return "missing";
  }
};

class CoderSession implements AdapterSession {
  readonly workspace: string;
  readonly shell: ReadOnlyShell;
  readonly runner: CommandRunner;
  readonly runs: RunOutcome[] = [];
  /** Bumped by every change to the copy, so a read or run after a change is not a repeat. */
  private revision = 0;
  private readonly appliedEdits = new Map<string, number>();
  private closed = false;

  constructor(readonly source: string, readonly copy: IsolatedCopy, private readonly options: CoderOptions) {
    this.workspace = copy.root;
    const extra = writableDirs(options.writable ?? [], source);
    this.shell = new ReadOnlyShell(copy.root, { readPaths: copy.commonDir ? [copy.commonDir] : [] }).enter();
    try {
      this.runner = new CommandRunner(copy.root, [copy.top, copy.tmp, ...extra], runEnvironment(process.env, copy.tmp), {
        privateDir: copy.privateDir, osSandbox: options.osSandbox ?? "auto", allowUnsandboxed: options.allowUnsandboxedRun === true,
        // A secret location that holds the workspace or the copy would hide the work itself.
        hidden: secretPaths(process.env).filter((secret) => escapesRoot(source, secret) && escapesRoot(copy.top, secret)),
      }).enter();
    } catch (error) {
      this.shell.close();
      throw error;
    }
  }

  private changed(): void {
    this.revision += 1;
    forgetWorkspaceIndex(this.workspace);
  }

  toolCall(action: Action, root: string, runtime: { tool_timeout_seconds: number }): ToolCall {
    const name = String(action.action);
    if (name === "search" || name === "read" || name === "list") {
      const call = structuredCall(this.shell, action, runtime.tool_timeout_seconds);
      return { ...call, signature: { ...call.signature, revision: this.revision } };
    }
    if (name === "edit") {
      const edit = { path: String(action.path), search: String(action.search), replace: String(action.replace) } satisfies EditAction;
      const key = JSON.stringify(edit);
      return {
        // The file's content is part of the identity: a failed edit may be retried once the file changed.
        signature: { edit, file: sha1(path.resolve(root, edit.path)) },
        run: async () => {
          const step = this.appliedEdits.get(key);
          if (step !== undefined) throw new ValidationError(`this exact edit was already applied (edit ${step}); read the file to see its current lines`);
          const result = applyEdit(root, edit);
          this.appliedEdits.set(key, this.appliedEdits.size + 1);
          this.changed();
          return result;
        },
        record: { name: "edit", raw_name: "edit", arguments: edit },
        ledger: (status) => ({ action: "edit", status, source: edit.path }),
      };
    }
    if (name === "write") {
      const write = { path: String(action.path), content: String(action.content) } satisfies WriteAction;
      const bytes = Buffer.byteLength(write.content, "utf8");
      return {
        signature: { write },
        run: async () => {
          const result = writeWholeFile(root, write);
          this.changed();
          return result;
        },
        // The content reaches the caller through the diff; the trail keeps its size.
        record: { name: "write", raw_name: "write", arguments: { path: write.path, content_bytes: bytes, content_limit_bytes: WRITE_MAX_BYTES } },
        ledger: (status) => ({ action: "write", status, source: write.path }),
      };
    }
    if (name === "run") {
      const requested = String(action.command);
      return {
        signature: { run: requested.trim(), revision: this.revision },
        run: async () => {
          const command = allowedCommand(requested, this.options.commands);
          const outcome = await this.runner.run(command, this.options.runTimeoutSeconds ?? DEFAULT_RUN_TIMEOUT_SECONDS);
          this.runs.push(outcome);
          this.changed();
          return describeRun(outcome);
        },
        record: { name: "run", raw_name: "run", arguments: { command: requested } },
        ledger: (status) => ({ action: "run", status, source: requested }),
      };
    }
    const commandText = String(action.command ?? "");
    return {
      signature: { command: commandText, revision: this.revision },
      run: () => this.shell.run(commandText, runtime.tool_timeout_seconds),
      record: { name: "shell", raw_name: action.action ?? "", arguments: { command: commandText } },
      ledger: (status) => ({ action: "shell", status, source: commandText }),
    };
  }

  /** The result's `coder` field: the agent's diff, the files it changed and its runs. */
  report(): Record<string, unknown> {
    const runs = this.runs.map((outcome, index) => ({
      command: outcome.command,
      exit_code: outcome.exitCode,
      ...(outcome.signal ? { signal: outcome.signal } : {}),
      timed_out: outcome.timedOut,
      duration_ms: outcome.durationMs,
      ...(index >= this.runs.length - KEPT_RUN_OUTPUTS ? { output: outcome.kept } : {}),
    }));
    const base = {
      isolation: this.copy.kind,
      base: { head: this.copy.head, caller_changes: this.copy.callerChanges },
      linked_dependencies: this.copy.linked,
      run_sandbox: this.runner.kind,
      runs,
    };
    try {
      const changes = agentChanges(this.copy);
      let patchFile: string | null = null;
      if (this.options.patchPath && changes.patch.length > 0) {
        mkdirSync(path.dirname(this.options.patchPath), { recursive: true });
        writeFileSync(this.options.patchPath, changes.patch, { mode: 0o600 });
        patchFile = this.options.patchPath;
      }
      return {
        coder: { ...base, diff: changes.patch.toString("utf8"), patch_bytes: changes.patch.length, patch_file: patchFile, changed_files: changes.files },
      };
    } catch (error) {
      return { coder: { ...base, diff: "", changed_files: [], error: `could not collect the changes: ${error instanceof Error ? error.message : String(error)}` } };
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.shell.close();
    removeIsolatedCopy(this.source, this.copy.container);
  }
}

/** The briefing: the workspace map, and the commands `run` accepts. */
function briefing(commands: string[]) {
  return (workspace: string): string => {
    const overview = workspaceOverview(workspace);
    const allowed = commands.length > 0
      ? `Allowed run commands, exactly as written: ${commands.map((command) => JSON.stringify(command)).join(", ")}.`
      : "No run commands are allowed in this task: you cannot test, so say the change is untested when you finish.";
    return overview ? `${overview}\n\n${allowed}` : allowed;
  };
}

export function coderAdapter(options: CoderOptions): AdapterSpec {
  const commands = normalizeAllowedCommands(options.commands);
  const settings = { ...options, commands };
  return {
    defaultToolTimeoutSeconds: 30,
    systemPrompt: () => CODER_SYSTEM_PROMPT,
    briefing: briefing(commands),
    parseAction,
    requireSource: false,
    open: (workspace) => {
      const container = settings.container ?? newCopyContainer();
      const copy = createIsolatedCopy(workspace, container, { maxCopyBytes: settings.maxCopyBytes, linkDependencies: settings.linkDependencies });
      try {
        return new CoderSession(realpathSync(workspace), copy, settings);
      } catch (error) {
        removeIsolatedCopy(workspace, copy.container);
        throw error;
      }
    },
    remaining: (count) => `\n\nActions remaining: ${count}. Finish once the change is made and tested; do not repeat prior actions.`,
    exhausted: "Action budget is exhausted. Return exactly a finish JSON action now: what changed, why, and the last test result. Do not request another action.",
    adapterMetadata: (session) => {
      const coder = session as CoderSession | undefined;
      return {
        adapter: "coder_agent_v1",
        sandbox: coder?.shell.kind ?? "restricted-bash",
        run_sandbox: coder?.runner.kind ?? "none",
        isolation: coder?.copy.kind ?? null,
      };
    },
    report: (session) => (session instanceof CoderSession ? session.report() : {}),
  };
}

if (isEntry(import.meta.url, "coder-agent")) {
  const result = process.env.FREELLAMA_AGENT_RESULT;
  const refuse = (message: string) => {
    process.stderr.write(`${message}\n`);
    if (result) writeFailureResult(result, message);
    process.exitCode = 1;
  };
  if (process.env[OPT_IN_ENV] !== "1") {
    refuse(`the coder adapter is off: the operator enables it with ${OPT_IN_ENV}=1 in the MCP server's environment`);
  } else {
    let options: CoderOptions | undefined;
    try {
      options = coderOptionsFromEnv(process.env);
    } catch (error) {
      refuse(`invalid coder adapter configuration: ${error instanceof Error ? error.message : String(error)}`);
    }
    if (options) process.exitCode = await runAdapter(coderAdapter(options));
  }
}
