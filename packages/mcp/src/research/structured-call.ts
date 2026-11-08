// The structured search, read and list actions as adapter tool calls, shared by the bash and coder
// adapters. An adapter entry module (bash-agent.ts) must not be imported by another adapter: its
// main guard compares the bundle's own URL and would start that adapter too.
import type { Action } from "./adapter.js";
import type { ReadOnlyShell } from "./sandbox.js";
import { listEntries, looksLikeRegex, readLines, resolveNamedPath, searchArgv, type ReadAction, type SearchAction } from "./structured-actions.js";

/**
 * A search that corrects the two mistakes models make most: a path written relative to a package
 * or taken from the question (resolved by suffix, else the whole workspace is searched), and a
 * regex such as `a|b` sent as a literal (retried as a regex when the literal finds nothing).
 */
async function structuredSearch(shell: ReadOnlyShell, action: SearchAction, timeoutSeconds: number): Promise<string> {
  const notes: string[] = [];
  const resolved = resolveNamedPath(shell.workspace, action.path, "any");
  let target = resolved.path;
  if (target === undefined) {
    notes.push(`(${resolved.note}; searched the whole workspace)`);
    target = ".";
  } else if (resolved.note) {
    notes.push(resolved.note);
  }
  const ripgrep = shell.hasTool("rg");
  const run = (regex: boolean | undefined) => shell.runTool(searchArgv(shell.workspace, { ...action, path: target, regex }, ripgrep), timeoutSeconds);
  let output = await run(action.regex);
  if (output === "(no matches)" && action.regex === undefined && looksLikeRegex(action.pattern)) {
    try {
      const asRegex = await run(true);
      if (asRegex !== "(no matches)") {
        notes.push("(no literal match; searched as a regular expression)");
        output = asRegex;
      }
    } catch {
      // Not a valid regex either: the literal result stands.
    }
  }
  return notes.length > 0 ? `${notes.join("\n")}\n${output}` : output;
}

/** A search, read or list: the harness builds the confined command; the model only names it. */
export function structuredCall(shell: ReadOnlyShell, action: Action, timeoutSeconds: number) {
  const name = String(action.action);
  const args: Record<string, unknown> = Object.fromEntries(Object.entries(action).filter(([key]) => key !== "action"));
  const run = async (): Promise<string> => {
    if (name === "read") return readLines(shell.workspace, args as ReadAction);
    if (name === "list") return listEntries(shell.workspace, args.path as string | undefined);
    return structuredSearch(shell, args as SearchAction, timeoutSeconds);
  };
  return {
    signature: { [name]: args },
    run,
    record: { name, raw_name: name, arguments: args },
    ledger: (status: string) => ({ action: name, status, source: JSON.stringify(args) }),
  };
}
