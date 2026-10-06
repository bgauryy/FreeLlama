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
  if (action !== "shell" && action !== "page" && action !== "finish") {
    throw new ValidationError(`unsupported action for bash adapter: ${String(action)}`);
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

export const BASH_ADAPTER: AdapterSpec = {
  defaultToolTimeoutSeconds: 30,
  systemPrompt: () => BASH_SYSTEM_PROMPT,
  parseAction,
  open: (workspace) => {
    const shell = new ReadOnlyShell(workspace).enter();
    return {
      shell,
      toolCall: (action, _workspace, runtime) => {
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
