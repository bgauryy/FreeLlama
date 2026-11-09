// The GitHub adapter's `gh` action: the GitHub CLI a model already knows, limited to read-only
// commands and run as an argument vector (no shell). It uses the operator's existing `gh` login.
import { ValidationError, ToolExitError } from "./errors.js";
import { runProcess } from "./proc.js";
import { trimSpace } from "./text.js";

/** Read-only `gh` commands: group → allowed subcommands (an empty set means the group itself). */
const READ_ONLY: Record<string, ReadonlySet<string>> = {
  search: new Set(["code", "repos", "issues", "prs", "commits"]),
  repo: new Set(["view", "list"]),
  pr: new Set(["view", "list", "diff", "checks"]),
  issue: new Set(["view", "list"]),
  release: new Set(["view", "list"]),
  api: new Set(),
};
/** Flags that write, send a body, change the method or open a browser. */
const FORBIDDEN = /^(?:-X|--method|-f|-F|--field|--raw-field|--input|-H|--header|-w|--web|--browser|-e|--edit|--hostname|--paginate-all)(?:=|$)/;
const MAX_ARGS = 40;

/** Throws unless `args` is one read-only `gh` invocation. */
export function validateGhArgs(args: unknown): string[] {
  if (!Array.isArray(args) || args.length === 0 || args.length > MAX_ARGS || !args.every((arg) => typeof arg === "string" && !arg.includes("\n"))) {
    throw new ValidationError(`gh action requires "args": an array of 1-${MAX_ARGS} strings, e.g. ["search","code","SystemOneRequest","--repo","ollama/ollama"]`);
  }
  const words = args as string[];
  const [group, sub] = words;
  const allowed = READ_ONLY[group!];
  if (!allowed) throw new ValidationError(`gh ${group} is not allowed; read-only commands: search, repo view/list, pr view/list/diff/checks, issue view/list, release view/list, api (GET)`);
  if (allowed.size > 0 && !allowed.has(sub ?? "")) {
    throw new ValidationError(`gh ${group} ${sub ?? ""} is not read-only; allowed: ${[...allowed].map((name) => `gh ${group} ${name}`).join(", ")}`);
  }
  const bad = words.find((word) => FORBIDDEN.test(word));
  if (bad) throw new ValidationError(`gh flag ${bad} is not allowed: the gh action only reads`);
  if (group === "api" && (!sub || sub.startsWith("-") || /graphql/i.test(sub))) {
    throw new ValidationError("gh api needs a REST path such as repos/ollama/ollama/contents/api/types.go (GET only; no graphql)");
  }
  return words;
}

/** Run one validated `gh` command; a search with no results is "(no results)". */
export async function runGh(args: string[], cwd: string, timeoutSeconds: number): Promise<string> {
  const result = await runProcess(["gh", ...validateGhArgs(args)], {
    cwd,
    // gh reads its login from the operator's config and keychain; nothing prompts.
    env: { ...process.env, GH_PROMPT_DISABLED: "1", GH_NO_UPDATE_NOTIFIER: "1", NO_COLOR: "1", GH_PAGER: "cat" },
    timeoutSeconds,
  });
  const output = trimSpace(result.stdout);
  if (result.returncode !== 0) throw new ToolExitError(result.returncode, `${output}\n${trimSpace(result.stderr)}`.trim());
  return output || "(no results)";
}
