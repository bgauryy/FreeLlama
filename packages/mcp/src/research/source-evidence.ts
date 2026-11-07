// Grounding requires a successful lookup with a source origin, not merely a successful process.
// Diagnostic/generative commands remain in the audit but cannot qualify a research answer.
import { realpathSync, statSync } from "node:fs";
import path from "node:path";
import { isRecord } from "./canonical-json.js";
import { segments } from "./sandbox-validate.js";

const LOOKUP_TOOLS = new Set([
  "structureSearch", "localSearch", "localFetch", "astSearch", "lspSearch",
  "ghSearchRepo", "artifactSearch", "ghStructure", "ghSearchCode", "ghGetFileContent", "ghSearchHistory", "ghGetHistoryItem",
]);
// Programmable filters can ignore stdin and synthesize their entire output (for example jq -n).
const FILTERS = new Set(["head", "tail", "grep", "egrep", "fgrep", "rg", "wc", "sort", "uniq", "cut", "nl", "tr", "column", "fold"]);
const READS = new Set(["cat", "head", "tail", "nl", "wc", "diff", "cmp", "file", "stat", "ls", "tree", "find"]);

function sourcePath(value: string | undefined, workspace: string): boolean {
  if (!value || value.startsWith("-") || /[*?$`]/.test(value)) return false;
  try {
    const root = realpathSync(workspace);
    const source = realpathSync(path.resolve(root, value));
    const relative = path.relative(root, source);
    if (relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) return false;
    const stat = statSync(source);
    return stat.isFile() || stat.isDirectory();
  } catch { return false; }
}

function searchReadsSource(words: string[], workspace: string): boolean {
  const positional: string[] = [];
  let files = false;
  let literal = false;
  for (const word of words.slice(1)) {
    if (!literal && word === "--") { literal = true; continue; }
    if (!literal && word === "--files") { files = true; continue; }
    if (!literal && word.startsWith("-")) {
      // Deliberately narrow: value-taking flags can otherwise make a pattern look like a path.
      if (!["-n", "-l", "-i", "-H", "-S", "-F", "--fixed-strings", "--line-number"].includes(word)) return false;
    } else positional.push(word);
  }
  return positional.length >= (files ? 1 : 2) && sourcePath(positional.at(-1), workspace);
}

export function sourceBearingLookup(call: unknown, workspace: string): boolean {
  if (!isRecord(call) || call.status !== "ok" || typeof call.result !== "string") return false;
  const output = call.result.trim();
  if (!output || /^\((?:no output|empty result)/i.test(output)) return false;
  const args = isRecord(call.arguments) ? call.arguments : {};
  if (typeof args.tool === "string" && LOOKUP_TOOLS.has(args.tool)) {
    if (!output.split("\n").some((line) => line.trim() && !line.startsWith("#"))) return false;
    const queries = Array.isArray(args.queries) ? args.queries : [args.queries];
    return queries.length > 0 && queries.every(isRecord);
  }
  if (call.raw_name !== "shell" || typeof args.command !== "string") return false;
  try {
    const pipeline = segments(args.command);
    const first = pipeline[0];
    if (!first || pipeline.slice(1).some((words) => !FILTERS.has(words[0] ?? ""))) return false;
    const [verb] = first;
    if (["rg", "grep", "egrep", "fgrep"].includes(verb ?? "")) return searchReadsSource(first, workspace);
    if (verb === "git" && first[1] === "grep") return searchReadsSource(first.slice(1), workspace);
    if (verb === "sed") return first.length === 4 && first[1] === "-n" && /^\d+(?:,\d+)?p$/.test(first[2]!) && sourcePath(first[3], workspace);
    if (!READS.has(verb ?? "")) return false;
    return sourcePath(verb === "find" ? first[1] : first.at(-1), workspace);
  } catch { return false; }
}
