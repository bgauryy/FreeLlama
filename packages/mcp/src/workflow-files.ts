// User-owned workflows: one Markdown file per workflow in FREELLAMA_HOME/workflows (or
// FREELLAMA_WORKFLOWS_DIR), its front matter naming the job whose model picks run it, its body the
// local model's instructions. A file named like a built-in workflow replaces it; every file is
// validated, and one that fails is skipped with the reason, never half-applied. Read once, when the
// MCP server or an agent command starts. local-prompts.ts merges these with the built-ins.
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";

/** What a user workflow file declares; the same shape as a built-in workflow, minus code. */
export type UserWorkflow = {
  job: string;
  title: string;
  input: string;
  system: string;
  format?: "json";
  prefer?: "capable";
  think?: boolean;
  maxOutput?: number;
  /** Where it came from and a short digest of its bytes, so a receipt names the exact version. */
  source: { path: string; digest: string };
};

export type WorkflowFileError = { path: string; error: string };

/** Lower-case words joined by hyphens: the name doubles as `freellama/<name>` and `-w <name>`. */
export const WORKFLOW_NAME = /^[a-z0-9][a-z0-9-]{1,47}$/;
const KEYS = new Set(["job", "title", "input", "format", "prefer", "think", "max_output"]);
const MAX_FILE_BYTES = 64 * 1024;

/** A front-matter value: quoted (a `#` inside is text) or bare, either one with an optional `# comment`. */
function scalar(raw: string): string {
  const value = raw.trim();
  const quoted = /^(["'])(.*?)\1\s*(?:#.*)?$/.exec(value);
  return quoted ? quoted[2]! : value.replace(/(?:^|\s+)#.*$/, "").trim();
}

/**
 * Parse one workflow file. `jobs` are the job names a workflow may run; `reserved` are names a
 * workflow may not take (jobs and `auto` share the `freellama/` namespace).
 */
export function parseWorkflowFile(name: string, text: string, jobs: readonly string[], reserved: readonly string[]): Omit<UserWorkflow, "source"> {
  if (!WORKFLOW_NAME.test(name)) throw new Error(`the file name must be lower-case letters, digits and hyphens (2-48), like summarize-meeting.md`);
  if (reserved.includes(name)) throw new Error(`"${name}" is a job or virtual model name; choose another file name`);
  // A byte-order mark (Windows editors add one) is not part of the front matter.
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/.exec(text.replace(/^\uFEFF/, ""));
  if (!match) throw new Error("the file must start with front matter between --- lines (job, title), followed by the instructions");
  const fields: Record<string, string> = {};
  for (const line of match[1]!.split(/\r?\n/)) {
    if (!line.trim() || line.trim().startsWith("#")) continue;
    const pair = /^([a-z_]+)\s*:\s*(.*)$/.exec(line.trim());
    if (!pair) throw new Error(`front matter line "${line.trim()}" is not "key: value"`);
    const [, key, value] = pair;
    if (!KEYS.has(key!)) throw new Error(`unknown front matter key "${key}"; allowed: ${[...KEYS].join(", ")}`);
    if (key! in fields) throw new Error(`front matter key "${key}" appears twice`);
    fields[key!] = scalar(value!);
  }
  const system = match[2]!.trim();
  if (!system) throw new Error("the instructions after the front matter are empty");
  if (!fields.job) throw new Error(`front matter needs "job:" (${jobs.join(", ")})`);
  if (!jobs.includes(fields.job)) throw new Error(`job "${fields.job}" is not one of ${jobs.join(", ")}`);
  if (!fields.title) throw new Error('front matter needs "title:", one line for the calling agent');
  const workflow: Omit<UserWorkflow, "source"> = {
    job: fields.job,
    title: fields.title,
    input: fields.input || "files: the input; prompt: the request (optional)",
    system,
  };
  if (fields.format !== undefined) {
    if (fields.format !== "json") throw new Error('format must be "json"');
    workflow.format = "json";
  }
  if (fields.prefer !== undefined) {
    if (fields.prefer !== "capable") throw new Error('prefer must be "capable" (the strongest pick that fits now)');
    workflow.prefer = "capable";
  }
  if (fields.think !== undefined) {
    if (fields.think !== "true" && fields.think !== "false") throw new Error("think must be true or false");
    workflow.think = fields.think === "true";
  }
  if (fields.max_output !== undefined) {
    const limit = Number(fields.max_output);
    if (!Number.isInteger(limit) || limit < 16 || limit > 32_768) throw new Error("max_output must be a whole number of tokens from 16 to 32768");
    workflow.maxOutput = limit;
  }
  return workflow;
}

/** The workflows directory: FREELLAMA_WORKFLOWS_DIR, else FREELLAMA_HOME/workflows. */
export function workflowsDir(home: string, env: NodeJS.ProcessEnv = process.env): string {
  return env.FREELLAMA_WORKFLOWS_DIR || path.join(home, "workflows");
}

/** Every `*.md` file in `dir`, parsed; a missing directory is no workflows, not an error. */
export function loadWorkflowFiles(dir: string, jobs: readonly string[], reserved: readonly string[]): { workflows: Map<string, UserWorkflow>; errors: WorkflowFileError[] } {
  const workflows = new Map<string, UserWorkflow>();
  const errors: WorkflowFileError[] = [];
  let names: string[];
  try {
    names = readdirSync(dir).filter((file) => file.endsWith(".md")).sort();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") errors.push({ path: dir, error: (error as Error).message });
    return { workflows, errors };
  }
  for (const file of names) {
    const full = path.join(dir, file);
    try {
      const bytes = readFileSync(full);
      if (bytes.length > MAX_FILE_BYTES) throw new Error(`larger than ${MAX_FILE_BYTES / 1024} KiB`);
      const parsed = parseWorkflowFile(file.slice(0, -3), bytes.toString("utf8"), jobs, reserved);
      workflows.set(file.slice(0, -3), { ...parsed, source: { path: full, digest: createHash("sha256").update(bytes).digest("hex").slice(0, 12) } });
    } catch (error) {
      errors.push({ path: full, error: (error as Error).message });
    }
  }
  return { workflows, errors };
}
