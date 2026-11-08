// MCP prompts: one per job, each a short user message holding the exact call to make, plus the
// `freellama` guide. The agent-facing texts are in local-prompts.ts; this file only builds calls.
import path from "node:path";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { AGENT_CHECKS, AGENT_GUIDE, APPLY_EDITS, TOOL_JOBS, WORKFLOWS, type WorkflowName } from "./local-prompts.js";
import { expandFileEntries } from "./task-files.js";

type Args = Record<string, unknown>;

/** Paths from a prompt argument, separated by commas or new lines; a glob stays one entry. */
const list = (value: string) => value.split(/[,\n]/).map((entry) => entry.trim()).filter(Boolean);

/** A path as a call names it: relative to the server's working directory when under it. */
function short(file: string): string {
  const relative = path.relative(process.cwd(), path.resolve(file));
  return relative && !relative.startsWith("..") && !path.isAbsolute(relative) ? relative : file;
}

/** One answer per file: a single call for one file, else `tasks` of up to 64 items (globs expanded here). */
export function perFile(files: string, shared: Args, item: Args = {}): { args: Args; note?: string } {
  let paths = list(files);
  try {
    paths = [...new Set(expandFileEntries(paths).map(short))];
  } catch {
    // A glob that matches nothing stays as written; run_task names the problem.
  }
  if (paths.length <= 1) return { args: { ...shared, ...item, files: paths } };
  const shown = paths.slice(0, 64);
  return {
    args: { ...shared, tasks: shown.map((file) => ({ id: file, independent: true, task: { ...item, files: [file] } })) },
    note: paths.length > shown.length ? `${paths.length - shown.length} more files: send them in further calls of up to 64 items.` : undefined,
  };
}

const call = (tool: string, args: Args) => `${tool} ${JSON.stringify(args)}`;
function message(...lines: Array<string | undefined>) {
  return { messages: [{ role: "user" as const, content: { type: "text" as const, text: lines.filter(Boolean).join("\n") } }] };
}

const filesArg = z.string().describe("paths or a glob, comma-separated");
const researchModel = TOOL_JOBS["research-agent"].defaultPicks[0]!;
const otherPicks = (job: string) => `If that model is not installed or does not fit, models {"view":"catalog","job":"${job}"} lists the alternatives.`;

/** Every prompt: name, description, arguments and the message it returns. */
export const BUILTIN_PROMPTS = {
  classify: {
    description: "Label or yes/no-check each file with the local decision job",
    argsSchema: { files: filesArg, question: z.string(), labels: z.string().optional().describe("comma-separated; omit for yes/no") },
    build: ({ files, question, labels }: { files: string; question: string; labels?: string }) => {
      const options = labels ? list(labels) : [];
      const questions = options.length > 0
        ? { label: { type: "choice", instructions: question, criteria: { ...Object.fromEntries(options.map((option) => [option, option])), ...(options.includes("other") ? {} : { other: "None of these fits" }) } } }
        : { answer: { type: "noul", instructions: question } };
      const { args, note } = perFile(files, { job: "classify", questions });
      return message("Classify each file on a local decision model. Call:", call("run_task", args), note, RESULT_LINE);
    },
  },
  "find-code": {
    description: "Find the code an issue or question is about, ranked by a local embedding model",
    argsSchema: { question: z.string().describe("the issue, or what the code does"), files: filesArg },
    build: ({ question, files }: { question: string; files: string }) => message(
      "Rank the code against the question on a local embedding model. Call:",
      call("run_task", { job: "find-code", files: list(files), nearest: { query: question, top: 5 } }), RESULT_LINE),
  },
  duplicates: {
    description: "Find likely duplicate texts with a local embedding model",
    argsSchema: { files: filesArg },
    build: ({ files }: { files: string }) => message(
      "Find the most similar pairs on a local embedding model. Call:",
      call("run_task", { job: "duplicates", files: list(files), nearest: { top: 10 } }), RESULT_LINE),
  },
  research: {
    description: "Ask a local research agent a question about a code tree",
    argsSchema: { question: z.string(), workspacePath: z.string().optional().describe("absolute project path; default the server's working directory") },
    build: ({ question, workspacePath }: { question: string; workspacePath?: string }) => message(
      "Ask a local research agent. Call:",
      call("delegate_research", { question, workspacePath: workspacePath ?? process.cwd(), model: researchModel }),
      otherPicks("research-agent"), AGENT_CHECKS.research),
  },
  "github-research": {
    description: "Ask a local research agent a question about a GitHub repository, without cloning it",
    argsSchema: { question: z.string(), repo: z.string().optional().describe("owner/repo") },
    build: ({ question, repo }: { question: string; repo?: string }) => message(
      "Ask a local research agent that reads GitHub. Call:",
      call("delegate_research", { question: repo ? `In ${repo}: ${question}` : question, adapter: "github", model: researchModel }),
      otherPicks("research-agent"), AGENT_CHECKS.research),
  },
  sweep: {
    description: "Check a large code tree or log chunk by chunk in the background",
    argsSchema: { files: filesArg, question: z.string().describe("a yes/no question about one chunk, e.g. Does this code swallow an error?") },
    build: ({ files, question }: { files: string; question: string }) => message(
      "Sweep the files chunk by chunk on a local decision model. Call:",
      call("sweep", { action: "start", files: list(files), model: TOOL_JOBS.sweep.defaultPicks[0], questions: { flag: { type: "noul", instructions: question } } }),
      'Then sweep {"action":"wait","sweepId":"<the id it returns>"} until it finishes, and sweep {"action":"results","sweepId":"<id>","filter":"flagged"}.',
      AGENT_CHECKS.sweep),
  },
} as const;

/** What every run_task prompt ends with: the result says what to check. */
const RESULT_LINE = "The result ends with check: lines worked out from it; act on them first.";

/**
 * One prompt per workflow, built from local-prompts.ts: the call that runs it on a local model.
 * Per-file workflows (reading images, reviewing files) run one task per file.
 */
export function workflowPrompt(name: WorkflowName, files: string, request?: string) {
  const flow = WORKFLOWS[name];
  const perFileJobs = new Set(["ocr", "image-recognition"]);
  const item = request ? { prompt: request } : {};
  const { args, note } = perFileJobs.has(flow.job) ? perFile(files, { workflow: name }, item) : { args: { workflow: name, files: list(files), ...item }, note: undefined };
  return message(`${flow.title} on a local model. Call:`, call("run_task", args), note,
    flow.job === "coding" && name !== "review-code" && name !== "explain-code" ? APPLY_EDITS : undefined, RESULT_LINE);
}

export function registerBuiltinPrompts(server: McpServer): void {
  for (const [name, flow] of Object.entries(WORKFLOWS) as Array<[WorkflowName, (typeof WORKFLOWS)[WorkflowName]]>) {
    server.registerPrompt(name, {
      description: `${flow.title} (${flow.input})`,
      argsSchema: { files: filesArg, request: z.string().optional().describe("the question, change, symptom or field names") },
    }, ((args: { files: string; request?: string }) => workflowPrompt(name, args.files, args.request)) as never);
  }
  for (const [name, prompt] of Object.entries(BUILTIN_PROMPTS)) {
    server.registerPrompt(name, { description: prompt.description, argsSchema: prompt.argsSchema },
      ((args: never) => prompt.build(args)) as never);
  }
  server.registerPrompt("freellama", { description: "When and how to offload work to FreeLlama's local models" }, () => message(AGENT_GUIDE));
}
