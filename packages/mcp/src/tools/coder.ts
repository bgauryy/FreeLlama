// delegate_research `adapter:"coder"`: the MCP side of the coder adapter (src/research/coder-agent.ts).
// The operator opts in; each run gets its own copy directory and patch file, the copy is removed
// after the adapter exits however it ended (a SIGKILL at the timeout skips the adapter's own
// cleanup), and the result carries the agent's diff for the caller to review and `git apply`.
import { existsSync, rmSync } from "node:fs";
import { mkdtemp, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import * as z from "zod";
import { costTelemetry, structuredResult } from "../helpers.js";
import { removeIsolatedCopy } from "../research/isolated-copy.js";
import { EXTERNAL_COST } from "../telemetry.js";

export const CODER_OPT_IN_ENV = "FREELLAMA_MCP_ALLOW_CODER";
/** A coder edits, runs and fixes: more turns and time than a lookup. */
export const CODER_DEFAULT_MAX_TURNS = 16;
export const CODER_MIN_TIMEOUT_SECONDS = 600;
/** The inline diff stops here; the patch file always holds all of it. */
const DIFF_INLINE_CHARS = 200_000;

// Entries are checked in `coderRefusal`, which keeps the advertised schema small.
export const coderCommandsSchema = z.array(z.string()).max(10).optional().describe("exact commands coder may run");

/** Why a coder request is refused before anything runs, or undefined. */
export function coderRefusal(adapter: string, commands: string[] | undefined, env: NodeJS.ProcessEnv = process.env): Error | undefined {
  if (adapter !== "coder") {
    return commands === undefined ? undefined : new Error("agent.commands applies only to adapter \"coder\".");
  }
  if (env[CODER_OPT_IN_ENV] !== "1") {
    return new Error(
      `adapter "coder" is off. It edits files and runs commands (in an isolated copy, never the user's files), so the operator ` +
        `enables it with ${CODER_OPT_IN_ENV}=1 in the MCP server's environment. Ask the user, or make the change yourself.`,
    );
  }
  if (commands?.some((command) => !command.trim() || command.length > 500 || /[\n\r\0]/.test(command))) {
    return new Error("each agent.commands entry must be one non-empty line of at most 500 characters.");
  }
  return undefined;
}

export interface CoderRun {
  /** Adapter environment: the allowed commands, the copy directory and the patch path. */
  env: Record<string, string>;
  patchPath: string;
  /** Remove the copy (and the patch directory when no patch was written). Never throws. */
  cleanup: (workspace: string) => void;
}

export async function prepareCoderRun(commands: string[] | undefined): Promise<CoderRun> {
  const container = await realpath(await mkdtemp(path.join(tmpdir(), "freellama-coder-")));
  const patchDir = await realpath(await mkdtemp(path.join(tmpdir(), "freellama-patch-")));
  const patchPath = path.join(patchDir, "coder.patch");
  return {
    env: {
      FREELLAMA_CODER_COMMANDS: JSON.stringify(commands ?? []),
      FREELLAMA_CODER_COPY_DIR: container,
      FREELLAMA_CODER_PATCH: patchPath,
    },
    patchPath,
    cleanup: (workspace) => {
      removeIsolatedCopy(workspace, container);
      if (!existsSync(patchPath)) rmSync(patchDir, { recursive: true, force: true });
    },
  };
}

const coderReportSchema = z.object({
  isolation: z.string().optional(),
  run_sandbox: z.string().optional(),
  diff: z.string().default(""),
  patch_file: z.string().nullable().optional(),
  changed_files: z.array(z.object({ path: z.string(), status: z.string() }).loose()).default([]),
  runs: z.array(z.object({ command: z.string(), exit_code: z.number().nullable().optional(), timed_out: z.boolean().optional() }).loose()).default([]),
  error: z.string().optional(),
}).loose();

const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

interface CoderResultInput {
  result: { final_answer: string; tool_calls: unknown[]; usage: { input_tokens?: number | null; output_tokens?: number | null }; model_metadata?: Record<string, unknown> } & Record<string, unknown>;
  adapterError: unknown;
  evidence: unknown[];
  observations: unknown[];
  model: string;
  workspace: string;
}

/** The tool result of one coder run: the answer, the diff to review and how to apply it. */
export function coderToolResult(input: CoderResultInput) {
  const { result, model, workspace } = input;
  const parsed = coderReportSchema.safeParse(result.coder ?? {});
  const coder = parsed.success ? parsed.data : coderReportSchema.parse({});
  const patchFile = coder.patch_file && existsSync(coder.patch_file) ? coder.patch_file : null;
  const applyCommand = patchFile ? `git -C ${quote(workspace)} apply ${quote(patchFile)}` : null;
  const lastRun = coder.runs.at(-1);
  const failure = input.adapterError ? `coder adapter failed: ${result.final_answer}` : coder.error;
  const verification = {
    recommendation: input.adapterError ? "escalate" : "review",
    grounded: false,
    why: input.adapterError
      ? "The coder run failed; any diff is partial."
      : "A local model's candidate patch: read the diff and run the tests yourself before applying it.",
  };
  const runText = lastRun
    ? `last run: ${lastRun.command} -> ${lastRun.timed_out ? "timed out" : `exit ${String(lastRun.exit_code ?? "?")}`} (${coder.runs.length} run(s))`
    : "not run";
  const changedText = coder.changed_files.length > 0
    ? `changed: ${coder.changed_files.map((file) => `${file.path} (${file.status})`).join(", ")}`
    : "no file changes";
  const summary = `Coder ${input.adapterError ? "stopped" : "finished"}: ${coder.changed_files.length} file(s) changed; ${runText}; ` +
    `${result.usage.input_tokens ?? "?"} input / ${result.usage.output_tokens ?? "?"} output local tokens. ` +
    (applyCommand ? "Review structuredContent.diff, then apply it." : "Nothing to apply.");
  const diffTruncated = coder.diff.length > DIFF_INLINE_CHARS;
  const payload = {
    adapter: "coder",
    verification,
    answer: result.final_answer,
    summary,
    ...(failure ? { error: failure } : {}),
    diff: diffTruncated ? `${coder.diff.slice(0, DIFF_INLINE_CHARS)}\n… diff truncated; the patch file holds all of it …\n` : coder.diff,
    ...(diffTruncated ? { diffTruncated: true } : {}),
    changedFiles: coder.changed_files,
    patchFile,
    applyCommand,
    runs: coder.runs,
    isolation: coder.isolation ?? null,
    runSandbox: coder.run_sandbox ?? null,
    toolCallCount: result.tool_calls.length,
    usage: { inputTokens: result.usage.input_tokens ?? null, outputTokens: result.usage.output_tokens ?? null },
    telemetry: costTelemetry({ inputTokens: result.usage.input_tokens ?? null, outputTokens: result.usage.output_tokens ?? null }, EXTERNAL_COST),
    contextManagement: result.model_metadata?.context_management ?? null,
    execution: { receipts: result.model_metadata?.execution_receipts ?? [] },
    evidence: input.evidence,
    observations: input.observations,
  };
  const text = `${failure ? `${failure}\n\n` : ""}${input.adapterError ? "" : `${result.final_answer}\n\n`}${changedText}\n${runText}\n` +
    (applyCommand ? `apply with: ${applyCommand}\n` : "") +
    `(${model}, ${result.tool_calls.length} tool call(s); isolated ${coder.isolation ?? "copy"}, removed; run sandbox ${coder.run_sandbox ?? "none"})`;
  const shaped = structuredResult(payload, { text });
  return input.adapterError ? { ...shaped, isError: true } : shaped;
}
