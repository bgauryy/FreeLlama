// delegate_research progress: separate the adapter's progress lines from its stderr diagnostics
// and turn each structured event (src/research/progress-events.ts) into one MCP progress message.
// Messages are a view: the full commands and arguments stay in the result's evidence trail.
import path from "node:path";
import { PROGRESS_PREFIX } from "./research/progress-events.js";
import { shlexTokens } from "./research/shlex.js";
import type { ProgressSink } from "./progress.js";

type Fields = Record<string, unknown>;

const NEWLINE = 0x0a;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : undefined;
const integer = (value: unknown): number | undefined => (Number.isInteger(value) ? (value as number) : undefined);
const text = (value: unknown): string | undefined => (typeof value === "string" && value !== "" ? value : undefined);
const quoted = (value: string) => `'${value}'`;

/**
 * A byte stream filter for the adapter's stderr: complete lines that start with `prefix` go to
 * `onLine` (without the prefix and newline); every other byte is returned for capture unchanged.
 */
export class PrefixedLineFilter {
  private readonly prefix: Buffer;
  private partial: Buffer = Buffer.alloc(0);

  constructor(prefix: string, private readonly onLine: (payload: string) => void) {
    this.prefix = Buffer.from(prefix, "utf8");
  }

  /** Bytes held back until their line ends. */
  get pending(): number {
    return this.partial.length;
  }

  /** Feed one chunk; returns the diagnostic bytes it completes. */
  push(chunk: Buffer): Buffer {
    const data = this.partial.length > 0 ? Buffer.concat([this.partial, chunk]) : chunk;
    const end = data.lastIndexOf(NEWLINE);
    if (end < 0) {
      this.partial = data;
      return Buffer.alloc(0);
    }
    this.partial = data.subarray(end + 1);
    const kept: Buffer[] = [];
    let start = 0;
    while (start <= end) {
      const next = data.indexOf(NEWLINE, start);
      const line = data.subarray(start, next + 1);
      if (!this.take(line.subarray(0, line.length - 1))) kept.push(line);
      start = next + 1;
    }
    return Buffer.concat(kept);
  }

  /** End of stream: a final unterminated line is a progress line or diagnostics like any other. */
  end(): Buffer {
    const rest = this.partial;
    this.partial = Buffer.alloc(0);
    return rest.length > 0 && this.take(rest) ? Buffer.alloc(0) : rest;
  }

  private take(line: Buffer): boolean {
    if (!line.subarray(0, this.prefix.length).equals(this.prefix)) return false;
    try {
      this.onLine(line.subarray(this.prefix.length).toString("utf8"));
    } catch {
      // A malformed line or a failed report never fails the research run.
    }
    return true;
  }
}

/** A workspace path as the caller wrote it relative to the workspace, or as given if outside. */
function shown(value: unknown, workspace: string): string {
  const raw = text(value);
  if (!raw) return ".";
  if (!path.isAbsolute(raw)) return raw;
  const relative = path.relative(workspace, raw);
  if (relative === "") return ".";
  return relative.startsWith("..") || path.isAbsolute(relative) ? raw : relative;
}

/** Short options whose value is the next token (or the rest of the cluster), per search tool. */
const SEARCH_VALUE_SHORT = "efmABCdDgtTMj";
const SEARCH_VALUE_LONG = new Set([
  "--regexp", "--file", "--max-count", "--after-context", "--before-context", "--context", "--include",
  "--exclude", "--exclude-dir", "--glob", "--iglob", "--type", "--type-not", "--max-columns", "--threads",
]);

/** `grep 'pattern' in paths` for a single grep/rg/git grep command, or undefined if unsure. */
function describeSearch(verb: string, args: string[]): string | undefined {
  let pattern: string | undefined;
  const positional: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]!;
    if (arg === "--") {
      positional.push(...args.slice(index + 1));
      break;
    }
    if (arg.startsWith("--")) {
      const [name, inline] = arg.split(/=(.*)/s, 2) as [string, string | undefined];
      const value = inline ?? (SEARCH_VALUE_LONG.has(name) ? args[++index] : undefined);
      if (name === "--regexp") pattern ??= value;
      continue;
    }
    if (arg.startsWith("-") && arg.length > 1) {
      for (let at = 1; at < arg.length; at += 1) {
        if (!SEARCH_VALUE_SHORT.includes(arg[at]!)) continue;
        const value = at + 1 < arg.length ? arg.slice(at + 1) : args[++index];
        if (arg[at] === "e") pattern ??= value;
        break;
      }
      continue;
    }
    positional.push(arg);
  }
  if (pattern === undefined) pattern = positional.shift();
  if (pattern === undefined) return undefined;
  return `${verb} ${quoted(pattern)} in ${positional.length > 0 ? positional.join(" ") : "."}`;
}

const options = (args: string[]) => args.filter((arg) => arg.startsWith("-"));
const operands = (args: string[]) => args.filter((arg) => !arg.startsWith("-"));

/** A readable form of one shell command: common reads and searches, else the command itself. */
export function describeShellCommand(command: string): string {
  const literal = `$ ${command}`;
  let tokens: string[];
  try {
    tokens = shlexTokens(command, { posix: true, punctuationChars: ";&|()<>", whitespaceSplit: true, commenters: "" });
  } catch {
    return literal;
  }
  // Pipelines, lists and redirections read best as written.
  if (tokens.length === 0 || tokens.some((token) => /^[;&|()<>]+$/.test(token))) return literal;
  const [verb, ...args] = tokens as [string, ...string[]];
  switch (verb) {
    case "grep": case "egrep": case "fgrep": case "rg":
      return describeSearch(verb, args) ?? literal;
    case "git":
      return args[0] === "grep" ? describeSearch("git grep", args.slice(1)) ?? literal : literal;
    case "cat":
      return options(args).length === 0 && args.length > 0 ? `read ${args.join(", ")}` : literal;
    case "sed": {
      const range = args.length === 3 && args[0] === "-n" ? /^(\d+)(?:,(\d+))?p$/.exec(args[1]!) : null;
      return range ? `read ${args[2]} (lines ${range[1]}-${range[2] ?? range[1]})` : literal;
    }
    case "head": case "tail": {
      const files = operands(args);
      const flags = options(args);
      const count = flags.length === 0 ? "10"
        : flags.length === 1 && /^-n?\d+$/.test(flags[0]!) ? flags[0]!.replace(/^-n?/, "")
          : flags.length === 1 && flags[0] === "-n" ? undefined : null;
      // `head -n 40 file`: the count is the first operand.
      const lines = count === undefined && /^\d+$/.test(files[0] ?? "") ? files.shift() : count;
      if (typeof lines !== "string" || files.length !== 1) return literal;
      return verb === "head" ? `read ${files[0]} (lines 1-${lines})` : `read ${files[0]} (last ${lines} lines)`;
    }
    case "ls": case "tree": {
      const paths = operands(args);
      return `list ${paths.length > 0 ? paths.join(", ") : "."}`;
    }
    default:
      return literal;
  }
}

/** A readable form of one octocode tool call, paths shown relative to the workspace. */
export function describeOctocodeCall(tool: string, queries: Fields, workspace: string): string {
  const where = shown(queries.path ?? queries.uri, workspace);
  switch (tool) {
    case "localSearchCode": {
      const term = text(queries.keywords) ?? text(queries.pattern);
      return term ? `grep ${quoted(term)} in ${where}` : `search ${where}`;
    }
    case "localGetFileContent": {
      const start = integer(queries.startLine);
      const end = integer(queries.endLine);
      const match = text(queries.matchString);
      const detail = start !== undefined && end !== undefined ? ` (lines ${start}-${end})`
        : match ? ` (around ${quoted(match)})`
          : queries.minify === "symbols" ? " (outline)" : "";
      return `read ${where}${detail}`;
    }
    case "localViewStructure": {
      const depth = integer(queries.maxDepth);
      return `list ${where}${depth !== undefined ? ` (depth ${depth})` : ""}`;
    }
    case "localFindFiles": {
      const names = Array.isArray(queries.names) ? queries.names.filter((name) => typeof name === "string") : [];
      const what = names.length > 0 ? names.join(", ") : text(queries.pathPattern) ?? text(queries.regex);
      return what ? `find ${what} in ${where}` : `find files in ${where}`;
    }
    case "lspGetSemantics": {
      const symbol = text(queries.symbolName);
      return `lsp ${text(queries.type) ?? "query"}${symbol ? ` ${quoted(symbol)}` : ""} in ${where}`;
    }
    default:
      return `${tool} ${JSON.stringify(queries)}`;
  }
}

function describeAction(action: Fields, workspace: string): string {
  switch (action.action) {
    case "shell":
      return describeShellCommand(String(action.command ?? ""));
    case "octocode":
      return describeOctocodeCall(String(action.tool ?? "?"), object(action.queries) ?? {}, workspace);
    case "page":
      return `page ${String(action.page)} of step ${String(action.step)}`;
    default:
      return JSON.stringify(action);
  }
}

/** One progress message for an adapter event, or undefined for an event this server does not know. */
export function researchProgressMessage(value: unknown, workspace: string): string | undefined {
  const event = object(value);
  if (!event) return undefined;
  const turn = integer(event.turn);
  const maxTurns = integer(event.maxTurns);
  const at = turn !== undefined && maxTurns !== undefined ? `turn ${turn}/${maxTurns}` : undefined;
  switch (event.event) {
    case "model":
      return at && `${at}: model thinking`;
    case "retry":
      return `model server busy; retrying (attempt ${String(event.attempt)}/${String(event.attempts)})`;
    case "repair":
      return at && `${at}: reply was not a valid action; asking again`;
    case "action": {
      const action = object(event.action);
      if (!at || !action) return undefined;
      return `${at}: ${describeAction(action, workspace)}${event.repeat === true ? " (repeat; not re-run)" : ""}`;
    }
    case "finish":
      return "finishing answer";
    case "finalize":
      return `turn budget used${maxTurns !== undefined ? ` (${maxTurns})` : ""}; finishing answer`;
    case "job_phase": {
      const message = text(event.message);
      return message && `${at ?? "final answer"}: ${message}`;
    }
    default:
      return undefined;
  }
}

/**
 * The stderr filter that reports each adapter progress line to `report`. When `openJobs` is given
 * it holds the deferred jobs the adapter has open, for {@link cancelOpenJobs} after it exits.
 */
export function researchProgressFilter(report: ProgressSink, workspace: string, openJobs?: Set<string>): PrefixedLineFilter {
  return new PrefixedLineFilter(PROGRESS_PREFIX, (payload) => {
    const event: unknown = JSON.parse(payload);
    const fields = object(event);
    const id = text(fields?.id);
    if (id && fields?.event === "job_open") openJobs?.add(id);
    if (id && fields?.event === "job_closed") openJobs?.delete(id);
    const message = researchProgressMessage(event, workspace);
    if (message) report(message);
  });
}

/**
 * Cancel, best-effort and without waiting, every job an exited adapter left open: it was killed
 * (timeout, client cancel) before it could cancel the job itself.
 */
export function cancelOpenJobs(openJobs: Set<string>, cancel: (id: string) => Promise<unknown>): void {
  for (const id of openJobs) {
    try {
      void cancel(id).catch(() => undefined);
    } catch {
      // A failed cancel leaves the job to its own deadline.
    }
  }
  openJobs.clear();
}
