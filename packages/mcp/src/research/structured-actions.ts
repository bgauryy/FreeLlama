// Structured research actions: the model names what it wants (a search, a file range, a listing)
// and the harness builds the confined, correct command. Small local models otherwise lose turns
// to shell syntax (grep flags passed to rg, quoting, blocked operators) instead of researching.
import { existsSync, readdirSync, readFileSync, realpathSync, statSync, type Dirent } from "node:fs";
import path from "node:path";
import { ValidationError } from "./errors.js";
import { escapesRoot } from "./workspace-path.js";

/** Trees no answer lives in; also skipped by `list`. */
export const SKIPPED_DIRS = ["node_modules", "target", "dist", "build", ".git", ".venv", "venv", "vendor", "__pycache__", ".octocode", "site-packages", "coverage"];
/** Lines one `read` returns when the caller gives no end. */
export const READ_DEFAULT_LINES = 120;
const READ_MAX_LINES = 400;
const LIST_MAX_ENTRIES = 300;
const SEARCH_MAX_PER_FILE = 20;

export type SearchAction = { pattern: string; path?: string; glob?: string; regex?: boolean; ignoreCase?: boolean };
export type ReadAction = { path: string; start?: number; end?: number };

/** `rel` resolved inside `workspace`, as a path relative to it ("." for the root). */
export function confined(workspace: string, rel: string | undefined): string {
  const raw = (rel ?? ".").trim() || ".";
  if (path.isAbsolute(raw) && !raw.startsWith(workspace)) throw new ValidationError(`path is outside the workspace: ${raw}`);
  const absolute = path.resolve(workspace, raw);
  const real = existsSync(absolute) ? realpathSync(absolute) : absolute;
  if (escapesRoot(real, workspace)) throw new ValidationError(`path is outside the workspace: ${raw}`);
  return path.relative(workspace, real) || ".";
}

export function validateSearch(value: Record<string, unknown>): void {
  if (typeof value.pattern !== "string" || !value.pattern.trim()) throw new ValidationError("search action requires a non-empty string pattern");
  if (value.pattern.length > 300) throw new ValidationError("search pattern is longer than 300 characters");
  for (const key of ["path", "glob"]) {
    if (value[key] !== undefined && typeof value[key] !== "string") throw new ValidationError(`search ${key} must be a string`);
  }
  for (const key of ["regex", "ignoreCase"]) {
    if (value[key] !== undefined && typeof value[key] !== "boolean") throw new ValidationError(`search ${key} must be true or false`);
  }
}

export function validateRead(value: Record<string, unknown>): void {
  if (typeof value.path !== "string" || !value.path.trim()) throw new ValidationError("read action requires a file path");
  for (const key of ["start", "end"]) {
    const n = value[key];
    if (n !== undefined && (typeof n !== "number" || !Number.isInteger(n) || n < 1)) throw new ValidationError(`read ${key} must be a positive integer line number`);
  }
}

/**
 * The argument vector for a search: ripgrep when present (it honors .gitignore), else grep. A
 * literal pattern by default, because models write `foo(` or `a.b` meaning the text itself.
 */
export function searchArgv(workspace: string, action: SearchAction, ripgrep: boolean): string[] {
  const target = confined(workspace, action.path);
  if (ripgrep) {
    return ["rg", "-n", "--no-heading", "--color", "never", "--max-columns", "300", "--max-count", String(SEARCH_MAX_PER_FILE),
      ...(action.regex ? [] : ["-F"]), ...(action.ignoreCase ? ["-i"] : []),
      ...(action.glob ? ["-g", action.glob] : []), ...SKIPPED_DIRS.flatMap((dir) => ["-g", `!${dir}/`]),
      "-e", action.pattern, "--", target];
  }
  return ["grep", "-rnI", ...(action.regex ? ["-E"] : ["-F"]), ...(action.ignoreCase ? ["-i"] : []),
    ...(action.glob ? [`--include=${action.glob}`] : []), ...SKIPPED_DIRS.map((dir) => `--exclude-dir=${dir}`),
    "-e", action.pattern, "--", target];
}

/** Numbered lines `start`..`end` of one workspace file. */
export function readLines(workspace: string, action: ReadAction): string {
  const resolved = resolveNamedPath(workspace, action.path, "file");
  if (resolved.path === undefined) throw new ValidationError(`no such file: ${resolved.note}; search or list to find the right path`);
  const rel = resolved.path;
  const absolute = path.join(workspace, rel);
  if (!statSync(absolute).isFile()) throw new ValidationError(`${rel} is a directory; use list`);
  const bytes = readFileSync(absolute);
  if (bytes.includes(0)) throw new ValidationError(`${rel} is binary`);
  const text = bytes.toString("utf8");
  // A final newline ends the last line; it does not start another.
  const lines = (text.endsWith("\n") ? text.slice(0, -1) : text).split("\n");
  const start = Math.min(action.start ?? 1, Math.max(lines.length, 1));
  const end = Math.min(action.end ?? start + READ_DEFAULT_LINES - 1, start + READ_MAX_LINES - 1, lines.length);
  const width = String(end).length;
  const body = lines.slice(start - 1, end).map((line, index) => `${String(start + index).padStart(width)}  ${line}`).join("\n");
  const more = end < lines.length ? `\n(${rel} has ${lines.length} lines; read start=${end + 1} for more)` : "";
  return `${resolved.note ? `${resolved.note}\n` : ""}${rel}:${start}-${end}\n${body}${more}`;
}

/** Files and directories under `path`, two levels deep, skipping vendored and build trees. */
export function listEntries(workspace: string, rel: string | undefined): string {
  const resolved = resolveNamedPath(workspace, rel, "dir");
  if (resolved.path === undefined) throw new ValidationError(`not a directory: ${resolved.note}`);
  const base = resolved.path;
  const root = path.join(workspace, base);
  if (!statSync(root).isDirectory()) throw new ValidationError(`not a directory: ${base}; read it instead`);
  const out: string[] = [];
  const walk = (dir: string, depth: number) => {
    let entries: Dirent[];
    try {
      entries = readdirSync(dir, { withFileTypes: true }).sort((left, right) => left.name.localeCompare(right.name));
    } catch {
      return;
    }
    for (const entry of entries) {
      if (out.length >= LIST_MAX_ENTRIES) return;
      if (entry.name.startsWith(".") || SKIPPED_DIRS.includes(entry.name)) continue;
      const shown = path.relative(workspace, path.join(dir, entry.name));
      out.push(entry.isDirectory() ? `${shown}/` : shown);
      if (entry.isDirectory() && depth < 1) walk(path.join(dir, entry.name), depth + 1);
    }
  };
  walk(root, 0);
  const listing = out.length === 0 ? `(${base} is empty)` : out.join("\n") + (out.length >= LIST_MAX_ENTRIES ? "\n(listing truncated; list a subdirectory)" : "");
  return resolved.note ? `${resolved.note}\n${listing}` : listing;
}

const INDEX_MAX_ENTRIES = 50_000;
const indexes = new Map<string, { files: string[]; dirs: string[] }>();

/** Every file and directory under `workspace` (relative paths), skipping vendored trees; cached. */
export function workspaceIndex(workspace: string): { files: string[]; dirs: string[] } {
  const cached = indexes.get(workspace);
  if (cached) return cached;
  const files: string[] = [];
  const dirs: string[] = [];
  const walk = (dir: string) => {
    let entries: Dirent[];
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (files.length + dirs.length >= INDEX_MAX_ENTRIES) return;
      if (entry.name.startsWith(".") || SKIPPED_DIRS.includes(entry.name)) continue;
      const absolute = path.join(dir, entry.name);
      const rel = path.relative(workspace, absolute);
      if (entry.isDirectory()) {
        dirs.push(rel);
        walk(absolute);
      } else if (entry.isFile()) {
        files.push(rel);
      }
    }
  };
  walk(workspace);
  const index = { files, dirs };
  indexes.set(workspace, index);
  return index;
}

/**
 * Where a path the model named probably is: entries whose path ends with it (`routing/profile.rs`
 * for `packages/rust-core/src/platform/routing/profile.rs`), shortest first. Models write paths
 * relative to a crate or package, or from the question, not from the workspace root.
 */
export function pathCandidates(workspace: string, rel: string, kind: "file" | "dir" | "any" = "any"): string[] {
  const wanted = rel.replace(/^\.\//, "").replace(/\/+$/, "");
  if (!wanted || wanted === ".") return [];
  const { files, dirs } = workspaceIndex(workspace);
  const pool = kind === "file" ? files : kind === "dir" ? dirs : [...files, ...dirs];
  const suffix = `/${wanted}`;
  return pool.filter((candidate) => candidate === wanted || candidate.endsWith(suffix)).sort((a, b) => a.length - b.length);
}

/** A path the model named, resolved: the path itself when it exists, else its single suffix match. */
export function resolveNamedPath(workspace: string, rel: string | undefined, kind: "file" | "dir" | "any"):
  { path: string; note?: string } | { path: undefined; note: string } {
  const named = confined(workspace, rel);
  if (named === "." || existsSync(path.join(workspace, named))) return { path: named };
  const candidates = pathCandidates(workspace, named, kind);
  if (candidates.length === 1) return { path: candidates[0]!, note: `(${named} does not exist; using ${candidates[0]})` };
  if (candidates.length > 1) {
    return { path: undefined, note: `${named} does not exist; matching paths: ${candidates.slice(0, 8).join(", ")}${candidates.length > 8 ? ", …" : ""}` };
  }
  return { path: undefined, note: `${named} does not exist and no path ends with it` };
}

/** Whether a literal pattern looks like a regex the model meant (alternation, classes, anchors). */
export function looksLikeRegex(pattern: string): boolean {
  return /\||\.\*|\\[bswdBSWD]|\[[^\]]+\]|^\^|\$$|\(\?|\.\+/.test(pattern);
}
