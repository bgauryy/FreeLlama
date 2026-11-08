// A short map of the workspace, given to the research model with the question: what the top level
// holds and which file types dominate. Without it a small model guesses the language from the
// question (searching `*.rs` in a TypeScript folder) and spends its command budget on empty finds.
import { readdirSync, type Dirent } from "node:fs";
import path from "node:path";

const SKIP = new Set(["node_modules", "target", ".git", "dist", "build", ".venv", "venv", "vendor", "__pycache__", ".octocode", "site-packages", "coverage"]);
const MAX_ENTRIES = 20_000;
const MAX_DEPTH = 8;

export function workspaceOverview(root: string): string {
  let top: Dirent[];
  try {
    top = readdirSync(root, { withFileTypes: true });
  } catch {
    return "";
  }
  const extensions = new Map<string, number>();
  let files = 0;
  let seen = 0;
  const walk = (dir: string, depth: number) => {
    if (depth > MAX_DEPTH || seen > MAX_ENTRIES) return;
    let entries: Dirent[];
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (++seen > MAX_ENTRIES) return;
      if (entry.isDirectory()) {
        if (!SKIP.has(entry.name) && !entry.name.startsWith(".")) walk(path.join(dir, entry.name), depth + 1);
      } else if (entry.isFile()) {
        files += 1;
        const extension = path.extname(entry.name).toLowerCase() || "(none)";
        extensions.set(extension, (extensions.get(extension) ?? 0) + 1);
      }
    }
  };
  walk(root, 0);
  const listing = top
    .filter((entry) => !SKIP.has(entry.name) && !entry.name.startsWith("."))
    .sort((left, right) => Number(right.isDirectory()) - Number(left.isDirectory()) || left.name.localeCompare(right.name))
    .slice(0, 25)
    .map((entry) => (entry.isDirectory() ? `${entry.name}/` : entry.name));
  const types = [...extensions.entries()].sort((left, right) => right[1] - left[1]).slice(0, 8)
    .map(([extension, count]) => `${extension} ${count}`);
  return `Workspace overview (generated, not a search result): top level: ${listing.join(", ") || "(empty)"}${top.length > 25 ? ", …" : ""}. `
    + `${files}${seen > MAX_ENTRIES ? "+" : ""} files; most common types: ${types.join(", ") || "none"}. `
    + "Search the types that exist here; start with a content search such as grep -rn \"KEYWORD\" . or rg -n \"KEYWORD\".";
}
