// `saveAs`: one local result written as Markdown under ${FREELLAMA_HOME}/handoffs, so a later task
// (another local agent) reads it with `files` instead of the caller pasting it back as its own tokens.
import { lstatSync, mkdirSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
import { FREELLAMA_HOME } from "./config.js";
import { fileRoots, within } from "./task-files.js";

/** Where handoffs live; task-files.ts `fileRoots` lists the same directory as readable. */
export const HANDOFF_DIR = path.join(FREELLAMA_HOME, "handoffs");
export const HANDOFF_NAME = /^[a-z0-9-]{1,60}$/;

/** The opening lines of an answer, for `## Summary`: at most `lines` non-empty lines and 600 characters. */
export function summaryOf(text: string, lines = 5): string {
  const kept = text.split("\n").filter((line) => line.trim()).slice(0, lines).join("\n");
  return kept.length > 600 ? `${kept.slice(0, 600)}…` : kept;
}

type HandoffContent = { source: string; summary: string; result: string };

function handoffBody(title: string, content: HandoffContent): string {
  return `# ${title}\n\n${content.source}, ${new Date().toISOString()}.\n\n## Summary\n\n${content.summary.trim() || "(empty)"}\n\n## Result\n\n${content.result.trim() || "(empty)"}\n`;
}

/** Write `<dir>/<name>.md` (replacing an earlier handoff of that name) and return its path. */
export function writeHandoff(name: string, content: HandoffContent, dir = HANDOFF_DIR): string {
  if (!HANDOFF_NAME.test(name)) throw new Error("saveAs is 1 to 60 characters of a-z, 0-9 and -");
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  const file = path.join(dir, `${name}.md`);
  writeFileSync(file, handoffBody(name, content), { mode: 0o600 });
  return file;
}

/** `file` as the filesystem resolves it: symlinks followed as far as the path exists, the rest appended. */
function resolveThroughSymlinks(file: string): string {
  try {
    return realpathSync(file);
  } catch {
    const parent = path.dirname(file);
    return parent === file ? file : path.join(resolveThroughSymlinks(parent), path.basename(file));
  }
}

/**
 * Where an `outputPath` answer would go, or why it may not: inside the readable roots after
 * symlinks resolve, outside any `.git` directory, and either new or an existing regular file (the
 * caller owns the path and may replace it). Returns the resolved path.
 */
export function checkOutputPath(name: string, roots: string[] = fileRoots()): string {
  const given = path.resolve(name);
  const file = resolveThroughSymlinks(given);
  if ([given, file].some((candidate) => candidate.split(path.sep).includes(".git"))) {
    throw new Error(`outputPath ${name} is inside a .git directory; FreeLlama does not write there`);
  }
  if (!roots.some((root) => within(file, root))) {
    throw new Error(`outputPath ${name} is outside the writable roots (${roots.join(", ")}); use a path inside the project or temp directory, or add its directory to FREELLAMA_FILE_ROOTS`);
  }
  let exists = true;
  try {
    lstatSync(file);
  } catch {
    exists = false;
  }
  if (exists && !statSync(file).isFile()) throw new Error(`outputPath ${name} exists and is not a regular file`);
  return file;
}

/** Write the answer as a handoff-format Markdown file at `name` (see checkOutputPath), creating or replacing it; returns its resolved path. */
export function writeOutputFile(name: string, content: HandoffContent, roots: string[] = fileRoots()): string {
  const file = checkOutputPath(name, roots);
  mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  writeFileSync(file, handoffBody(path.basename(file), content), { mode: 0o600 });
  return file;
}

/** Where answers go when the view is shortened and the caller gave no `outputPath`; task-files.ts `fileRoots` lists it as readable. */
export const OUTPUT_TMP_DIR = path.join(FREELLAMA_HOME, "tmp");

const DAY_MS = 86_400_000;

/** Days a default output file lives: FREELLAMA_RESPONSE_TTL_DAYS as the response store reads it (default 7; 0 keeps them). */
function outputTtlDays(env: NodeJS.ProcessEnv = process.env): number {
  const configured = Number(env.FREELLAMA_RESPONSE_TTL_DAYS ?? 7);
  return Number.isFinite(configured) && configured >= 0 ? configured : 7;
}

/** Remove `.md` files in `dir` last modified more than `ttlDays` ago. A file that cannot be inspected or removed is skipped. */
export function pruneOutputDir(dir: string, now: number, ttlDays: number): void {
  if (ttlDays <= 0) return;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return;
  }
  for (const name of names.filter((entry) => entry.endsWith(".md"))) {
    const file = path.join(dir, name);
    try {
      if (statSync(file).mtimeMs < now - ttlDays * DAY_MS) rmSync(file);
    } catch {
      // Another process may have taken it.
    }
  }
}

let outputCounter = 0;

/**
 * Write the answer to a new `<dir>/<ISO time>-<pid>-<n>.md` (never replacing a file), after
 * pruning files past their TTL, and return its path.
 */
export function writeDefaultOutput(
  content: HandoffContent, dir = OUTPUT_TMP_DIR, now = new Date(), ttlDays = outputTtlDays(),
): string {
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  pruneOutputDir(dir, now.getTime(), ttlDays);
  outputCounter += 1;
  const file = path.join(dir, `${now.toISOString().replace(/[:.]/g, "-")}-${process.pid}-${outputCounter}.md`);
  writeFileSync(file, handoffBody(path.basename(file), content), { mode: 0o600, flag: "wx" });
  return file;
}
