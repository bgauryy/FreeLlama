// Octocode research adapters: a local Ollama model whose only research tools are the installed
// `octocode` CLI's read-only tools. One adapter reads the workspace; `github-agent` reads GitHub.
// Same adapter contract and decoding settings as the restricted-shell adapter; the only variable
// is the tool surface. Tools that write (ghCloneRepo, graphBuild) or call models (clasify) are out.
//
// Run as `node octocode-agent.js` with FREELLAMA_TARGET_MODEL (or FREELLAMA_BENCH_MODEL),
// FREELLAMA_BENCH_WORKSPACE, FREELLAMA_BENCH_PROMPT and FREELLAMA_AGENT_RESULT set.
import { pathToFileURL } from "node:url";
import { type Action, type AdapterSpec, isPositiveInt } from "./adapter.js";
import { runAdapter } from "./agent-loop.js";
import { parseJsonAction } from "./actions.js";
import { canonicalJson, isRecord } from "./canonical-json.js";
import { ToolExitError, ValidationError } from "./errors.js";
import { runProcess } from "./proc.js";
import { githubSystemPrompt, octocodeSystemPrompt } from "./prompts.js";
import { trimSpace } from "./text.js";
import { escapesRoot, joinPath, resolvePath } from "./workspace-path.js";
import { workspaceOverview } from "./workspace-overview.js";
import { runGh, validateGhArgs } from "./gh-action.js";

export const OCTOCODE_TOOLS: ReadonlySet<string> = new Set([
  "structureSearch", "localSearch", "localFetch", "astSearch", "lspSearch",
]);
export const GITHUB_TOOLS: ReadonlySet<string> = new Set([
  "ghSearchRepo", "artifactSearch", "ghStructure", "ghSearchCode", "ghGetFileContent", "ghSearchHistory", "ghGetHistoryItem",
]);
/** Queries one CLI call accepts. */
const MAX_QUERIES = 5;

/** The installed CLI, never a download: the tool reads the user's workspace. */
export const OCTOCODE_BIN = process.env.FREELLAMA_AGENT_OCTOCODE_BIN ?? "octocode";

/** Resolve `value` against the workspace `root`, refusing escapes and git metadata. */
export function safeResolve(root: string, value: string): string {
  const candidate = resolvePath(joinPath(root, value));
  if (escapesRoot(candidate, root)) throw new ValidationError(`path escapes benchmark workspace: ${value}`);
  if (candidate.split("/").includes(".git")) throw new ValidationError("git metadata is outside the task surface");
  return candidate;
}

/** The action's queries as the CLI's array: a small model often sends one bare query object. */
export function queryList(queries: unknown): Array<Record<string, unknown>> {
  if (isRecord(queries)) return [queries];
  return Array.isArray(queries) ? queries.filter(isRecord) : [];
}

/**
 * The shapes small models naturally write, as the one action the loop runs. Measured: a local model
 * wrote `{"action":"lspSearch",...}` three times, spending its whole repair budget on the wrapper
 * rather than the research. A tool name is accepted as `action`, and query fields at the top level
 * become one query.
 */
function normalizeAction(value: Action, tools: ReadonlySet<string>): Action {
  const named = typeof value.action === "string" && tools.has(value.action) ? value.action : undefined;
  if (named === undefined && value.action !== "octocode") return value;
  const { action: _action, tool: named2, queries, ...fields } = value;
  const tool = named ?? named2;
  if (tool === undefined) return value;
  const query = queries ?? (Object.keys(fields).length > 0 ? fields : undefined);
  return { action: "octocode", tool, ...(query === undefined ? {} : { queries: query }) } as Action;
}

function actionParser(tools: ReadonlySet<string>, gh = false) {
  const names = [...tools].sort().join(", ");
  return (content: string): Action => {
    const value = normalizeAction(parseJsonAction(content), tools);
    const action = value.action;
    if (gh && action === "gh") {
      validateGhArgs(value.args);
      return value;
    }
    if (action === "octocode") {
      const tool = value.tool;
      if (typeof tool !== "string" || !tools.has(tool)) {
        throw new ValidationError(`octocode action requires one supported tool: ${names}`);
      }
      const queries = queryList(value.queries);
      if (queries.length === 0 || queries.length > MAX_QUERIES) {
        throw new ValidationError(`octocode action requires queries: 1 to ${MAX_QUERIES} query objects`);
      }
    }
    if (action === "page" && (!isPositiveInt(value.step) || !isPositiveInt(value.page))) {
      throw new ValidationError("page action requires positive integer step and page");
    }
    if (action === "finish" && typeof value.answer !== "string") {
      throw new ValidationError("finish action requires a string answer");
    }
    if (action !== "octocode" && action !== "page" && action !== "finish") {
      throw new ValidationError(`unsupported action for octocode adapter: ${String(action)}${gh ? " (use octocode, gh, page or finish)" : ""}`);
    }
    return value;
  };
}
export const parseAction = actionParser(OCTOCODE_TOOLS);

/** Run one octocode CLI tool; its result lines, then its `#` notes and any refusal. */
async function runCli(root: string, tool: string, queries: Array<Record<string, unknown>>, timeoutSeconds: number): Promise<string> {
  const result = await runProcess([OCTOCODE_BIN, tool, JSON.stringify({ queries })], { cwd: root, timeoutSeconds });
  const parts = [trimSpace(result.stdout), trimSpace(result.stderr)].filter(Boolean);
  const output = parts.join("\n") || "(empty result)";
  // A batch exits with its worst row. Preserve all rows for inspection, but do not count a
  // partially failed batch as a successful evidence step. Signals are failures too.
  if (result.returncode !== 0) {
    throw new ToolExitError(result.returncode, output, result.returncode === 1 ? "empty" : "error");
  }
  return output;
}

/** Run one local tool: every path must stay inside the workspace. */
export async function runOctocode(root: string, toolName: string, queries: unknown, timeoutSeconds: number): Promise<string> {
  if (!OCTOCODE_TOOLS.has(toolName)) throw new ValidationError(`unsupported octocode tool: ${toolName}`);
  const list = queryList(queries);
  for (const query of list) {
    if (typeof query.path === "string") safeResolve(root, query.path);
  }
  return runCli(root, toolName, list, timeoutSeconds);
}

/** Run one GitHub tool: read-only and remote, so nothing here reaches the workspace. */
export async function runGithub(root: string, toolName: string, queries: unknown, timeoutSeconds: number): Promise<string> {
  if (!GITHUB_TOOLS.has(toolName)) throw new ValidationError(`unsupported GitHub tool: ${toolName}`);
  return runCli(root, toolName, queryList(queries), timeoutSeconds);
}

function adapterFor(options: {
  tools: ReadonlySet<string>;
  prompt: (workspace: string) => string;
  run: typeof runOctocode;
  metadata: string;
  briefing?: (workspace: string) => string;
  /** Also accept the read-only `gh` action (GitHub adapter). */
  gh?: boolean;
}): AdapterSpec {
  return {
    defaultToolTimeoutSeconds: 45,
    systemPrompt: options.prompt,
    ...(options.briefing ? { briefing: options.briefing } : {}),
    parseAction: actionParser(options.tools, options.gh),
    open: () => ({
      toolCall: (action, workspace, runtime) => {
        if (options.gh && action.action === "gh") {
          const args = validateGhArgs(action.args);
          return {
            signature: { gh: args },
            run: () => runGh(args, workspace, runtime.tool_timeout_seconds),
            record: { name: "gh", raw_name: "gh", arguments: { args } },
            ledger: (status) => ({ action: "gh", status, source: args.join(" ") }),
          };
        }
        const toolName = String(action.tool ?? "");
        const queries = queryList(action.queries);
        return {
          signature: { tool: toolName, queries },
          run: () => options.run(workspace, toolName, queries, runtime.tool_timeout_seconds),
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
    adapterMetadata: () => ({ adapter: options.metadata }),
  };
}

export const OCTOCODE_ADAPTER = adapterFor({ tools: OCTOCODE_TOOLS, prompt: octocodeSystemPrompt, run: runOctocode, metadata: "octocode_cli_agent_v2", briefing: workspaceOverview });
export const GITHUB_ADAPTER = adapterFor({ tools: GITHUB_TOOLS, prompt: githubSystemPrompt, run: runGithub, metadata: "octocode_github_agent_v2", gh: true });

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runAdapter(OCTOCODE_ADAPTER);
}
