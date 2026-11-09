// The coder adapter's writing actions, all confined to its isolated copy (./isolated-copy.ts):
// `edit` (an exact SEARCH/REPLACE that must match once), `write` (a whole file, size-capped) and
// `run` (only a command the caller allowed, character for character, in an OS sandbox that may
// write the copy and its scratch directory and nothing else, with no network).
import { spawn, spawnSync } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { ValidationError } from "./errors.js";
import { which } from "./sandbox.js";
import { escapesRoot, resolvePath } from "./workspace-path.js";
import { resolveNamedPath } from "./structured-actions.js";

/** Largest file `write` creates or replaces. */
export const WRITE_MAX_BYTES = 256 * 1024;
export const MAX_ALLOWED_COMMANDS = 10;
export const DEFAULT_RUN_TIMEOUT_SECONDS = 300;
export const MAX_RUN_TIMEOUT_SECONDS = 1_800;
/** Lines of a run's output the model sees: the start, and more of the end, where failures are. */
export const OBSERVATION_LINES = { head: 10, tail: 40, lineChars: 240 };
/** Lines of a run's output kept in the result. */
export const RESULT_LINES = { head: 20, tail: 80, lineChars: 300 };
const CAPTURE_HEAD_BYTES = 256 * 1024;
const CAPTURE_TAIL_BYTES = 1024 * 1024;
const EDIT_PREVIEW_LINES = 12;

export type EditAction = { path: string; search: string; replace: string };
export type WriteAction = { path: string; content: string };

/**
 * `rel` as a file the agent may write: inside `root` after every existing symlink is resolved,
 * and not in a `.git` directory. `root` must be a physical path.
 */
export function confinedWritePath(root: string, rel: unknown): { rel: string; absolute: string } {
  if (typeof rel !== "string" || !rel.trim()) throw new ValidationError("a file path is required");
  const raw = rel.trim();
  if (raw.includes("\0")) throw new ValidationError("path contains a NUL byte");
  const lexical = path.resolve(root, raw);
  const relative = path.relative(root, lexical);
  if (!relative || relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new ValidationError(`path is outside the copy: ${raw}`);
  }
  if (relative.split(path.sep).includes(".git")) throw new ValidationError(`${relative} is inside .git, which is not editable`);
  const physical = resolvePath(lexical);
  if (escapesRoot(physical, root)) throw new ValidationError(`path leaves the copy through a symlink: ${raw}`);
  return { rel: relative, absolute: physical };
}

/** Non-overlapping occurrences of `needle` in `text`, as start offsets. */
function occurrences(text: string, needle: string): number[] {
  const found: number[] = [];
  for (let at = text.indexOf(needle); at >= 0; at = text.indexOf(needle, at + needle.length)) found.push(at);
  return found;
}

const lineAt = (text: string, offset: number) => text.slice(0, offset).split("\n").length;

/** The 1-based line where `search` matches `text` with each line's surrounding whitespace ignored, when it does exactly once. */
function looseMatchLine(text: string, search: string): number | undefined {
  const wanted = search.split("\n").map((part) => part.trim());
  while (wanted.length > 0 && wanted[wanted.length - 1] === "") wanted.pop();
  if (wanted.length === 0) return undefined;
  const lines = text.split("\n").map((part) => part.trim());
  const hits: number[] = [];
  for (let index = 0; index + wanted.length <= lines.length && hits.length < 2; index += 1) {
    if (wanted.every((part, offset) => lines[index + offset] === part)) hits.push(index + 1);
  }
  return hits.length === 1 ? hits[0] : undefined;
}

/** Numbered lines `from`..`to` (1-based, inclusive) of `text`, at most EDIT_PREVIEW_LINES. */
function numbered(text: string, from: number, to: number): string {
  const lines = text.split("\n");
  const end = Math.min(to, from + EDIT_PREVIEW_LINES - 1, lines.length);
  const width = String(end).length;
  return lines.slice(from - 1, end).map((part, index) => `${String(from + index).padStart(width)}  ${part}`).join("\n")
    + (end < to ? `\n(${to - end} more changed lines)` : "");
}

/** Replace the one occurrence of `action.search` in a file of the copy. */
export function applyEdit(root: string, action: EditAction): string {
  const resolved = resolveNamedPath(root, action.path, "file");
  if (resolved.path === undefined) throw new ValidationError(`no such file: ${resolved.note}; use write to create a file`);
  const { rel, absolute } = confinedWritePath(root, resolved.path);
  if (!statSync(absolute).isFile()) throw new ValidationError(`${rel} is not a file`);
  const bytes = readFileSync(absolute);
  if (bytes.includes(0)) throw new ValidationError(`${rel} is binary and cannot be edited`);
  const text = bytes.toString("utf8");
  // A CRLF file shows no \r in a read, so a search copied from one has bare newlines.
  const crlf = text.includes("\r\n") && !action.search.includes("\r");
  const search = crlf ? action.search.replaceAll("\n", "\r\n") : action.search;
  const replace = crlf ? action.replace.replaceAll("\n", "\r\n") : action.replace;
  const found = occurrences(text, search);
  if (found.length === 0) {
    const loose = looseMatchLine(text.replaceAll("\r\n", "\n"), action.search);
    throw new ValidationError(
      `search text not found in ${rel} (0 matches). ` +
        (loose ? `It matches line ${loose} if indentation is ignored: read those lines and copy them exactly. ` : "Read the lines again and copy them exactly, indentation included. ") +
        "Nothing was changed.",
    );
  }
  if (found.length > 1) {
    const lines = found.slice(0, 6).map((offset) => lineAt(text, offset)).join(", ");
    throw new ValidationError(`search text matches ${found.length} times in ${rel} (lines ${lines}${found.length > 6 ? ", …" : ""}); include more surrounding lines so it matches exactly once. Nothing was changed.`);
  }
  const at = found[0]!;
  const updated = text.slice(0, at) + replace + text.slice(at + search.length);
  writeFileSync(absolute, updated);
  const first = lineAt(updated, at);
  const removed = search.split("\n").length;
  const added = replace === "" ? 0 : replace.split("\n").length;
  const shown = updated.replaceAll("\r\n", "\n");
  return `${resolved.note ? `${resolved.note}\n` : ""}edited ${rel}: ${removed} line(s) at line ${first} replaced by ${added} line(s)` +
    (added > 0 ? `\n${numbered(shown, first, first + added - 1)}` : "");
}

/** Create or replace a whole file of the copy. */
export function writeWholeFile(root: string, action: WriteAction): string {
  const { rel, absolute } = confinedWritePath(root, action.path);
  const size = Buffer.byteLength(action.content, "utf8");
  if (size > WRITE_MAX_BYTES) throw new ValidationError(`content is ${size} bytes; write accepts at most ${WRITE_MAX_BYTES} (use edit for large files)`);
  const existed = existsSync(absolute);
  if (existed && !statSync(absolute).isFile()) throw new ValidationError(`${rel} is a directory`);
  mkdirSync(path.dirname(absolute), { recursive: true });
  writeFileSync(absolute, action.content);
  const lines = action.content === "" ? 0 : action.content.split("\n").length - (action.content.endsWith("\n") ? 1 : 0);
  return `${existed ? "replaced" : "created"} ${rel} (${lines} lines, ${size} bytes)`;
}

/** The allowed command `requested` names, compared exactly after trimming surrounding space. */
export function allowedCommand(requested: unknown, allowed: readonly string[]): string {
  if (typeof requested !== "string" || !requested.trim()) throw new ValidationError("run requires a command");
  if (allowed.length === 0) throw new ValidationError("no commands are allowed in this task, so nothing can be run; finish and say the change is untested");
  const match = allowed.find((command) => command === requested.trim());
  if (match === undefined) {
    throw new ValidationError(`command not allowed: ${JSON.stringify(requested)}. Run exactly one of: ${allowed.map((command) => JSON.stringify(command)).join(", ")}`);
  }
  return match;
}

/** Validate the caller's allowed commands: at most ten distinct, non-empty single-line strings. */
export function normalizeAllowedCommands(value: unknown): string[] {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.some((command) => typeof command !== "string")) throw new ValidationError("commands must be an array of strings");
  const commands = [...new Set((value as string[]).map((command) => command.trim()))];
  if (commands.some((command) => !command || /[\n\r\0]/.test(command))) throw new ValidationError("each command must be one non-empty line");
  if (commands.length > MAX_ALLOWED_COMMANDS) throw new ValidationError(`at most ${MAX_ALLOWED_COMMANDS} commands may be allowed`);
  return commands;
}

// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;
const lenient = new TextDecoder("utf-8", { fatal: false });

/**
 * The first `head` and last `tail` lines of `text`, each clipped to `lineChars`, with a
 * "… N lines omitted …" marker between them; `extraOmitted` lines were dropped before `text` was
 * captured (at `gapLine`, the line where they were) and count toward the marker.
 */
export function headTail(text: string, options: { head: number; tail: number; lineChars: number }, gap?: { line: number; lines: number }): string {
  const lines = text.endsWith("\n") ? text.slice(0, -1).split("\n") : text.split("\n");
  const clip = (part: string) => (part.length > options.lineChars ? `${part.slice(0, options.lineChars)}…` : part);
  const marker = (count: number) => `… ${count} lines omitted …`;
  if (lines.length <= options.head + options.tail) {
    if (!gap) return lines.map(clip).join("\n");
    return [...lines.slice(0, gap.line).map(clip), marker(gap.lines), ...lines.slice(gap.line).map(clip)].join("\n");
  }
  const cutEnd = lines.length - options.tail;
  const insideCut = gap !== undefined && gap.line >= options.head && gap.line <= cutEnd;
  const omitted = cutEnd - options.head + (insideCut ? gap.lines : 0);
  const kept = [...lines.slice(0, options.head).map(clip), marker(omitted), ...lines.slice(cutEnd).map(clip)];
  if (gap && !insideCut) {
    // A capture gap outside the cut (very long lines): mark it where it was.
    const at = gap.line < options.head ? gap.line : gap.line - cutEnd + options.head + 1;
    kept.splice(at, 0, marker(gap.lines));
  }
  return kept.join("\n");
}

/** Combined stdout and stderr, bounded: the first bytes and the last bytes, counting what fell between. */
class OutputCapture {
  private head: Buffer[] = [];
  private headBytes = 0;
  private tail: Buffer[] = [];
  private tailBytes = 0;
  droppedLines = 0;
  droppedBytes = 0;

  push(chunk: Buffer): void {
    if (this.headBytes < CAPTURE_HEAD_BYTES) {
      const taken = chunk.subarray(0, CAPTURE_HEAD_BYTES - this.headBytes);
      this.head.push(taken);
      this.headBytes += taken.length;
      chunk = chunk.subarray(taken.length);
    }
    if (chunk.length === 0) return;
    this.tail.push(chunk);
    this.tailBytes += chunk.length;
    while (this.tailBytes - this.tail[0]!.length >= CAPTURE_TAIL_BYTES) {
      const dropped = this.tail.shift()!;
      this.tailBytes -= dropped.length;
      this.droppedBytes += dropped.length;
      for (let at = dropped.indexOf(0x0a); at >= 0; at = dropped.indexOf(0x0a, at + 1)) this.droppedLines += 1;
    }
  }

  /** The decoded text, and where bytes were dropped (a line index into it) when they were. */
  text(): { text: string; gap?: { line: number; lines: number } } {
    const clean = (bytes: Buffer) => lenient.decode(bytes).replace(ANSI, "").replace(/\r\n?/g, "\n");
    const head = clean(Buffer.concat(this.head));
    const tail = clean(Buffer.concat(this.tail));
    if (this.droppedBytes === 0) return { text: head + tail };
    const headLines = head.split("\n").length;
    return { text: `${head}\n${tail}`, gap: { line: headLines, lines: this.droppedLines } };
  }
}

export interface RunOutcome {
  command: string;
  exitCode: number | null;
  signal: string | null;
  timedOut: boolean;
  durationMs: number;
  /** The output as the model saw it (OBSERVATION_LINES). */
  observed: string;
  /** The output kept in the result (RESULT_LINES). */
  kept: string;
}

/** Every descendant of `pid`, deepest last, from one `ps` listing. */
function descendants(pid: number): number[] {
  const listing = spawnSync("ps", ["-A", "-o", "pid=,ppid="], { encoding: "utf8" });
  if (listing.status !== 0) return [];
  const children = new Map<number, number[]>();
  for (const row of listing.stdout.split("\n")) {
    const [child, parent] = row.trim().split(/\s+/).map(Number);
    if (!child || parent === undefined || Number.isNaN(parent)) continue;
    children.set(parent, [...(children.get(parent) ?? []), child]);
  }
  const out: number[] = [];
  const queue = [pid];
  while (queue.length > 0) {
    for (const child of children.get(queue.shift()!) ?? []) {
      out.push(child);
      queue.push(child);
    }
  }
  return out;
}

function killTree(pid: number): void {
  for (const target of [pid, ...descendants(pid)]) {
    try {
      process.kill(target, "SIGKILL");
    } catch {
      // Already exited.
    }
  }
}

/** Credential and FreeLlama-state locations under the home directory that a run never reads. */
const SECRET_HOME_PATHS = [".ssh", ".aws", ".gnupg", ".config/gh", ".netrc", ".git-credentials", ".docker", ".kube", ".octocode", "Library/Keychains"];

/** The existing secret locations for `env` (its HOME, and FREELLAMA_HOME), by real path. */
export function secretPaths(env: NodeJS.ProcessEnv): string[] {
  const candidates = [
    ...(env.HOME ? SECRET_HOME_PATHS.map((rel) => path.join(env.HOME!, rel)) : []),
    ...(env.FREELLAMA_HOME ? [env.FREELLAMA_HOME] : []),
  ];
  const found = new Set<string>();
  for (const candidate of candidates) {
    try {
      found.add(realpathSync(candidate));
    } catch {
      // Not present on this host.
    }
  }
  return [...found];
}

function macosRunProfile(writable: string[], hidden: string[]): string {
  const quote = (value: string) => `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
  const unreadable = hidden.map((target) => ` (${lstatSync(target).isDirectory() ? "subpath" : "literal"} ${quote(target)})`).join("");
  return `(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write* (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/fd/")${writable.map((dir) => ` (subpath ${quote(dir)})`).join("")})
${unreadable ? `(deny file-read* file-write*${unreadable})\n` : ""}`;
}

/**
 * Runs the caller's allowed commands with `/bin/sh -c` in the copy. On macOS `sandbox-exec` and on
 * Linux `bwrap` let a command write only `writable` (the copy, its scratch directory and any
 * operator-named directory) and reach no network. Reads stay allowed, since toolchains live in the
 * home directory, except `hidden` (credentials and FreeLlama's own state): an allowed command runs
 * code the model wrote. Without either sandbox, `run` is refused unless the operator allowed it.
 */
export class CommandRunner {
  kind: "sandbox-exec" | "bwrap" | "none" = "none";
  private prefix: string[] = [];

  constructor(
    readonly cwd: string,
    readonly writable: string[],
    readonly env: NodeJS.ProcessEnv,
    private readonly options: { privateDir: string; osSandbox: "auto" | "off"; allowUnsandboxed: boolean; hidden?: string[] },
  ) {}

  enter(): this {
    if (this.options.osSandbox === "off") return this;
    const sandboxExec = which("sandbox-exec");
    if (sandboxExec) {
      // The profile lives outside every writable directory, so a command cannot loosen the next run.
      const profile = path.join(this.options.privateDir, "run.sb");
      writeFileSync(profile, macosRunProfile(this.writable, this.options.hidden ?? []), { mode: 0o600 });
      const prefix = [sandboxExec, "-f", profile];
      if (probeRun([...prefix, "/bin/sh", "-c", "true"], this.env, this.cwd)) {
        this.kind = "sandbox-exec";
        this.prefix = prefix;
        return this;
      }
    }
    const bwrap = which("bwrap");
    if (bwrap) {
      const hide = (this.options.hidden ?? []).flatMap((target) =>
        lstatSync(target).isDirectory() ? ["--tmpfs", target] : ["--ro-bind", "/dev/null", target]);
      const prefix = [bwrap, "--die-with-parent", "--new-session", "--unshare-all", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
        "--tmpfs", "/tmp", ...hide, ...this.writable.flatMap((dir) => ["--bind", dir, dir]), "--chdir", this.cwd, "--"];
      if (probeRun([...prefix, "/bin/sh", "-c", "true"], this.env, this.cwd)) {
        this.kind = "bwrap";
        this.prefix = prefix;
      }
    }
    return this;
  }

  async run(command: string, timeoutSeconds: number): Promise<RunOutcome> {
    if (this.kind === "none" && !this.options.allowUnsandboxed) {
      throw new ValidationError(
        "run is unavailable: this host has no OS sandbox (sandbox-exec or bwrap) to confine it, and the operator has not set " +
          "FREELLAMA_CODER_UNSANDBOXED_RUN=1. Finish and say the change is untested.",
      );
    }
    const started = performance.now();
    const capture = new OutputCapture();
    const argv = [...this.prefix, "/bin/sh", "-c", command];
    const outcome = await new Promise<{ code: number | null; signal: string | null; timedOut: boolean }>((resolve, reject) => {
      const child = spawn(argv[0]!, argv.slice(1), { cwd: this.cwd, env: this.env, stdio: ["ignore", "pipe", "pipe"] });
      let timedOut = false;
      let grace: NodeJS.Timeout | undefined;
      const settle = (code: number | null, signal: string | null) => {
        clearTimeout(timer);
        clearTimeout(grace);
        resolve({ code, signal, timedOut });
      };
      // A process that left the tree (a daemon) can hold the pipes open: stop reading shortly
      // after the shell itself has exited.
      const release = (code: number | null, signal: string | null) => {
        grace = setTimeout(() => {
          child.stdout!.destroy();
          child.stderr!.destroy();
          settle(code, signal);
        }, 2_000);
      };
      const timer = setTimeout(() => {
        timedOut = true;
        if (child.pid !== undefined) killTree(child.pid);
        release(null, "SIGKILL");
      }, Math.max(timeoutSeconds, 0.001) * 1000);
      child.stdout!.on("data", (chunk: Buffer) => capture.push(chunk));
      child.stderr!.on("data", (chunk: Buffer) => capture.push(chunk));
      child.once("error", (error) => {
        clearTimeout(timer);
        clearTimeout(grace);
        reject(error);
      });
      child.once("exit", (code, signal) => {
        if (grace === undefined) release(code, signal);
      });
      child.once("close", (code, signal) => settle(code, signal));
    });
    const { text, gap } = capture.text();
    return {
      command,
      exitCode: outcome.code,
      signal: outcome.signal,
      timedOut: outcome.timedOut,
      durationMs: Math.round(performance.now() - started),
      observed: headTail(text, OBSERVATION_LINES, gap),
      kept: headTail(text, RESULT_LINES, gap),
    };
  }
}

function probeRun(argv: string[], env: NodeJS.ProcessEnv, cwd: string): boolean {
  const result = spawnSync(argv[0]!, argv.slice(1), { cwd, env, stdio: ["ignore", "pipe", "pipe"], timeout: 10_000, killSignal: "SIGKILL" });
  return !result.error && result.status === 0;
}

/** How a run reads to the model: the command, how it ended, then its bounded output. */
export function describeRun(outcome: RunOutcome): string {
  const seconds = (outcome.durationMs / 1000).toFixed(1);
  const ended = outcome.timedOut ? `timed out after ${seconds} s and was killed`
    : outcome.exitCode !== null ? `exit code ${outcome.exitCode} (${seconds} s)` : `killed by ${outcome.signal ?? "a signal"} (${seconds} s)`;
  return `$ ${outcome.command}\n${ended}\n${outcome.observed.trim() ? outcome.observed : "(no output)"}`;
}

/** The environment of an allowed command: identity, locale and toolchain homes; no FreeLlama settings or secrets. */
export function runEnvironment(source: NodeJS.ProcessEnv, tmp: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  // Cache locations stay the user's: an offline run needs what they hold (corepack's yarn, cargo's
  // registry), and the sandbox keeps them read-only unless FREELLAMA_CODER_WRITABLE names them.
  for (const key of ["PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "CARGO_HOME", "RUSTUP_HOME", "COREPACK_HOME",
    "XDG_CACHE_HOME", "npm_config_cache"]) {
    if (source[key] !== undefined) env[key] = source[key];
  }
  return {
    ...env,
    PATH: env.PATH ?? "/usr/bin:/bin",
    TMPDIR: tmp, TMP: tmp, TEMP: tmp,
    CI: "1", NO_COLOR: "1", FORCE_COLOR: "0", TERM: "dumb",
    // The sandbox has no network: let cargo use its cache instead of failing on a registry update.
    CARGO_NET_OFFLINE: "true",
    GIT_TERMINAL_PROMPT: "0",
  };
}
