// Octocode research adapter: a local Ollama model whose only research tool is the `octocode` CLI.
// Same adapter contract and decoding settings as the restricted-shell adapter; the only variable
// is the tool surface.
//
// Run as `node octocode-agent.js` with FREELLAMA_TARGET_MODEL (or FREELLAMA_BENCH_MODEL),
// FREELLAMA_BENCH_WORKSPACE, FREELLAMA_BENCH_PROMPT and FREELLAMA_AGENT_RESULT set.
import { pathToFileURL } from "node:url";
import { type Action, type AdapterSpec, isPositiveInt } from "./adapter.js";
import { runAdapter } from "./agent-loop.js";
import { parseJsonAction } from "./actions.js";
import { canonicalJson, isRecord } from "./canonical-json.js";
import { ValidationError } from "./errors.js";
import { runProcess } from "./proc.js";
import { octocodeSystemPrompt } from "./prompts.js";
import { trimSpace } from "./text.js";
import { escapesRoot, joinPath, resolvePath } from "./workspace-path.js";

export const OCTOCODE_TOOLS: ReadonlySet<string> = new Set([
  "localViewStructure", "localFindFiles", "localSearchCode", "localGetFileContent", "lspGetSemantics",
]);
const PATH_KEYS = ["path", "uri"] as const;

// Pinned: an unpinned `npx octocode` downloads whatever is latest at call time (a supply-chain risk
// for a tool that reads the user's workspace), and the first download can exceed the tool timeout.
// Override deliberately with FREELLAMA_AGENT_OCTOCODE_PACKAGE.
export const OCTOCODE_PACKAGE = process.env.FREELLAMA_AGENT_OCTOCODE_PACKAGE ?? "octocode@19.1.0";

const SORTED_TOOLS = [...OCTOCODE_TOOLS].sort().join(", ");

/** Resolve `value` against the workspace `root`, refusing escapes and git metadata. */
export function safeResolve(root: string, value: string): string {
  const candidate = resolvePath(joinPath(root, value));
  if (escapesRoot(candidate, root)) throw new ValidationError(`path escapes benchmark workspace: ${value}`);
  if (candidate.split("/").includes(".git")) throw new ValidationError("git metadata is outside the task surface");
  return candidate;
}

export function parseAction(content: string): Action {
  const value = parseJsonAction(content);
  const action = value.action;
  if (action === "octocode") {
    const tool = value.tool;
    if (typeof tool !== "string" || !OCTOCODE_TOOLS.has(tool)) {
      throw new ValidationError(`octocode action requires one supported tool: ${SORTED_TOOLS}`);
    }
    if (!isRecord(value.queries)) throw new ValidationError("octocode action requires a queries object");
  }
  if (action === "page" && (!isPositiveInt(value.step) || !isPositiveInt(value.page))) {
    throw new ValidationError("page action requires positive integer step and page");
  }
  if (action === "finish" && typeof value.answer !== "string") {
    throw new ValidationError("finish action requires a string answer");
  }
  if (action !== "octocode" && action !== "page" && action !== "finish") {
    throw new ValidationError(`unsupported action for octocode adapter: ${String(action)}`);
  }
  return value;
}

/** Run one octocode CLI tool with workspace-resolved paths. */
export async function runOctocode(root: string, toolName: string, queries: Record<string, unknown>, timeoutSeconds: number): Promise<string> {
  if (!OCTOCODE_TOOLS.has(toolName)) throw new ValidationError(`unsupported octocode tool: ${toolName}`);
  const resolved: Record<string, unknown> = { ...queries };
  for (const key of PATH_KEYS) {
    const value = resolved[key];
    if (typeof value === "string") resolved[key] = safeResolve(root, value);
  }
  const command = ["npx", "--yes", OCTOCODE_PACKAGE, "tools", toolName, "--queries", JSON.stringify(resolved), "--compact"];
  const result = await runProcess(command, { cwd: root, timeoutSeconds });
  let output = result.stdout + (result.returncode !== 0 ? `\nSTDERR:\n${result.stderr}` : "");
  if (!trimSpace(output)) output = "(empty result)";
  return output;
}

export const OCTOCODE_ADAPTER: AdapterSpec = {
  defaultToolTimeoutSeconds: 45,
  systemPrompt: (workspace) => octocodeSystemPrompt(workspace),
  parseAction,
  open: () => ({
    toolCall: (action, workspace, runtime) => {
      const toolName = String(action.tool ?? "");
      const queries = isRecord(action.queries) ? action.queries : {};
      return {
        signature: { tool: toolName, queries },
        run: () => runOctocode(workspace, toolName, queries, runtime.tool_timeout_seconds),
        record: {
          name: toolName ? `mcp.octocode.${toolName}` : "mcp.octocode.unknown",
          raw_name: action.action ?? "",
          arguments: { tool: toolName, queries },
        },
        ledger: (status) => ({ action: toolName, status, source: canonicalJson(queries, { sortKeys: false, ensureAscii: false }) }),
      };
    },
  }),
  remaining: (count) => `\n\nTool calls remaining: ${count}. Finish now if the task is answerable; do not repeat prior calls.`,
  exhausted: "Tool budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another tool.",
  adapterMetadata: () => ({ adapter: "octocode_cli_agent_v1" }),
};

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runAdapter(OCTOCODE_ADAPTER);
}
