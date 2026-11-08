// Checks a research answer against the workspace: a file path the answer cites must exist. A small
// model can read the right grep hit and still write a plausible path that is not there; the
// calling agent would then open nothing, or trust a wrong location.
import { existsSync } from "node:fs";
import path from "node:path";

/** `src/a/b.ts`, `./x/y.rs:12`: a relative path with a directory and a file extension. */
const PATH_TOKEN = /(?<![\w@/.-])(?:\.\/)?[\w@-][\w@.-]*(?:\/[\w@.-]+)+\.[A-Za-z][A-Za-z0-9]{0,5}(?=$|[\s:`'"),.;\]])/g;

function clean(token: string): string {
  return token.replace(/^\.\//, "").replace(/[.,;]+$/, "");
}

/** Paths mentioned in `text`, cleaned and unique, in order. */
export function pathsIn(text: string): string[] {
  return [...new Set([...text.matchAll(PATH_TOKEN)].map((match) => clean(match[0])))];
}

/** Cited paths that do not exist under `workspace` (absolute paths and URLs are not judged). */
export function missingPaths(answer: string, workspace: string): string[] {
  return pathsIn(answer).filter((candidate) => !existsSync(path.join(workspace, candidate)));
}

/** Paths from the run's own observations that exist, for the repair message (at most `limit`). */
export function evidencePaths(observations: string[], workspace: string, limit = 10): string[] {
  const found: string[] = [];
  for (const text of observations) {
    for (const candidate of pathsIn(text)) {
      if (!found.includes(candidate) && existsSync(path.join(workspace, candidate))) found.push(candidate);
      if (found.length >= limit) return found;
    }
  }
  return found;
}

export function groundingRepairNotice(missing: string[], evidence: string[]): string {
  return `These paths in your answer do not exist in the workspace: ${missing.join(", ")}. `
    + (evidence.length > 0 ? `Paths your own commands returned: ${evidence.join(", ")}. ` : "")
    + "Return a finish action again that cites only paths that exist; if the evidence does not establish the answer, say so.";
}

export function groundingNote(missing: string[]): string {
  return `\n[freellama: cited path(s) not found in the workspace: ${missing.join(", ")}; treat this answer as unverified]`;
}

/** A hint for tool errors a model repeats when it mixes up grep and rg flags. */
export function toolErrorHint(message: string): string {
  if (/rg: unrecognized flag --(?:exclude-dir|include)/.test(message)) {
    return "\nHint: rg excludes directories with --glob '!node_modules' and filters types with -t ts; or use grep -rn PATTERN . --exclude-dir=node_modules.";
  }
  return "";
}

/**
 * `path:line` hits from successful search output whose file the answer names: a citation the caller
 * can open, for an answer that came from a grep hit without a separate read of the file.
 */
export function searchHitCitations(
  steps: { step: number; status: string; result: unknown }[], answer: string, workspace: string,
): { step: number; tool: "shell"; path: string; line: number; kind: "search_hit" }[] {
  const named = new Set(pathsIn(answer));
  const cited = new Map<string, { step: number; tool: "shell"; path: string; line: number; kind: "search_hit" }>();
  for (const step of steps) {
    if (step.status !== "ok" || typeof step.result !== "string") continue;
    for (const match of step.result.matchAll(/^(?:\.\/)?([^\s:]+):(\d+):/gm)) {
      const file = match[1]!;
      if (!named.has(file) || cited.has(file) || !existsSync(path.join(workspace, file))) continue;
      cited.set(file, { step: step.step, tool: "shell", path: file, line: Number(match[2]), kind: "search_hit" });
    }
  }
  return [...cited.values()];
}
