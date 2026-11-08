// Restricted-shell research adapter: a local Ollama model whose only tool is a read-only shell.
// Same adapter contract and decoding settings as the octocode adapter; the only variable is the
// tool surface. Validation, restricted bash, a scrubbed environment and (where the host allows)
// an OS sandbox live in ./sandbox.ts and ./sandbox-validate.ts.
//
// Run as `node bash-agent.js` with FREELLAMA_TARGET_MODEL (or FREELLAMA_BENCH_MODEL),
// FREELLAMA_BENCH_WORKSPACE, FREELLAMA_BENCH_PROMPT and FREELLAMA_AGENT_RESULT set.
import { pathToFileURL } from "node:url";
import { type Action, type AdapterSpec, isPositiveInt } from "./adapter.js";
import { runAdapter } from "./agent-loop.js";
import { parseJsonAction } from "./actions.js";
import { ValidationError } from "./errors.js";
import { BASH_SYSTEM_PROMPT } from "./prompts.js";
import { ReadOnlyShell, assertCommandConfined } from "./sandbox.js";
import { trimSpace } from "./text.js";
import { workspaceOverview } from "./workspace-overview.js";
import { listEntries, looksLikeRegex, readLines, resolveNamedPath, searchArgv, validateRead, validateSearch, type ReadAction, type SearchAction } from "./structured-actions.js";

export { assertCommandConfined };

export function parseAction(content: string): Action {
  const value = parseJsonAction(content);
  const action = value.action;
  if (action === "shell") {
    const command = value.command;
    if (typeof command !== "string" || !trimSpace(command)) throw new ValidationError("shell action requires a non-empty string command");
  }
  if (action === "page" && (!isPositiveInt(value.step) || !isPositiveInt(value.page))) {
    throw new ValidationError("page action requires positive integer step and page");
  }
  if (action === "finish" && typeof value.answer !== "string") {
    throw new ValidationError("finish action requires a string answer");
  }
  if (action === "search") validateSearch(value);
  if (action === "read") validateRead(value);
  if (action === "list" && value.path !== undefined && typeof value.path !== "string") throw new ValidationError("list path must be a string");
  if (!["shell", "search", "read", "list", "page", "finish"].includes(String(action))) {
    throw new ValidationError(`unsupported action for bash adapter: ${String(action)} (use search, read, list, shell, page or finish)`);
  }
  return value;
}

/** Run one command in `shell`, or in a scoped shell rooted at `root` when none is given. */
export async function runShell(root: string, commandText: string, timeoutSeconds: number, shell?: ReadOnlyShell): Promise<string> {
  if (shell) return shell.run(commandText, timeoutSeconds);
  const scoped = new ReadOnlyShell(root).enter();
  try {
    return await scoped.run(commandText, timeoutSeconds);
  } finally {
    scoped.close();
  }
}

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
function structuredCall(shell: ReadOnlyShell, action: Action, timeoutSeconds: number) {
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

export const BASH_ADAPTER: AdapterSpec = {
  defaultToolTimeoutSeconds: 30,
  systemPrompt: () => BASH_SYSTEM_PROMPT,
  briefing: workspaceOverview,
  parseAction,
  open: (workspace) => {
    const shell = new ReadOnlyShell(workspace).enter();
    return {
      shell,
      toolCall: (action, _workspace, runtime) => {
        if (action.action === "search" || action.action === "read" || action.action === "list") {
          return structuredCall(shell, action, runtime.tool_timeout_seconds);
        }
        const commandText = String(action.command ?? "");
        return {
          signature: { command: commandText },
          run: () => shell.run(commandText, runtime.tool_timeout_seconds),
          record: { name: "shell", raw_name: action.action ?? "", arguments: { command: commandText } },
          ledger: (status) => ({ action: "shell", status, source: commandText }),
        };
      },
      close: () => shell.close(),
    };
  },
  remaining: (count) => `\n\nCommands remaining: ${count}. Finish now if the task is answerable; do not repeat prior commands.`,
  exhausted: "Command budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another command.",
  adapterMetadata: (session) => ({
    adapter: "bash_shell_agent_v1",
    sandbox: (session as { shell?: ReadOnlyShell } | undefined)?.shell?.kind ?? "restricted-bash",
  }),
};

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runAdapter(BASH_ADAPTER);
}
