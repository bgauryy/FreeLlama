// `saveAs`: one local result written as Markdown under ${FREELLAMA_HOME}/handoffs, so a later task
// (another local agent) reads it with `files` instead of the caller pasting it back as its own tokens.
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { FREELLAMA_HOME } from "./config.js";

/** Where handoffs live; task-files.ts `fileRoots` lists the same directory as readable. */
export const HANDOFF_DIR = path.join(FREELLAMA_HOME, "handoffs");
export const HANDOFF_NAME = /^[a-z0-9-]{1,60}$/;

/** The opening lines of an answer, for `## Summary`: at most `lines` non-empty lines and 600 characters. */
export function summaryOf(text: string, lines = 5): string {
  const kept = text.split("\n").filter((line) => line.trim()).slice(0, lines).join("\n");
  return kept.length > 600 ? `${kept.slice(0, 600)}…` : kept;
}

/** Write `<dir>/<name>.md` (replacing an earlier handoff of that name) and return its path. */
export function writeHandoff(
  name: string, content: { source: string; summary: string; result: string }, dir = HANDOFF_DIR,
): string {
  if (!HANDOFF_NAME.test(name)) throw new Error("saveAs is 1 to 60 characters of a-z, 0-9 and -");
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  const file = path.join(dir, `${name}.md`);
  const body = `# ${name}\n\n${content.source}, ${new Date().toISOString()}.\n\n## Summary\n\n${content.summary.trim() || "(empty)"}\n\n## Result\n\n${content.result.trim() || "(empty)"}\n`;
  writeFileSync(file, body, { mode: 0o600 });
  return file;
}
