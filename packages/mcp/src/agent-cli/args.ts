// `freellama run` / `freellama chat` flags -> `run_task` arguments. Pure: no I/O except reading the
// `@file` values the caller names. `ollama run` flags keep their meaning; a flag whose effect a
// managed task cannot honor is refused with the command that can, never silently dropped.
import { existsSync, readFileSync, statSync } from "node:fs";
import { parseArgs, type ParseArgsConfig } from "node:util";
import { LOCAL_JOBS, RUN_TASK_JOBS, WORKFLOWS, type JobName, type WorkflowName } from "../local-prompts.js";
import { withoutUndefined } from "./util.js";

/** A caller mistake: printed as one line, exit code 2. */
export class UsageError extends Error {}

export type RenderOptions = {
  json: boolean; verbose: boolean; quiet: boolean; hideThinking: boolean; extract?: boolean;
  /** true: --stream, false: --no-stream, undefined: stream when stdout is a terminal. */
  stream?: boolean;
};

type RunPlan = {
  /** `run_task` arguments, validated later by the tool's own schema. */
  args: Record<string, unknown>;
  render: RenderOptions;
  /** No prompt, input, files or question: `run` on a TTY becomes a chat. */
  promptless: boolean;
  /** --each: one task per file named by -f, run as one batch. */
  each?: boolean;
  /** The model came from the first word (`freellama run WORD …`), so it may be a prompt after all. */
  positionalModel?: boolean;
};

/** Ollama attaches an image named in the prompt (`ollama run llava "what is in ./a.png?"`); so does freellama. */
const IMAGE_FILE = /\.(?:png|jpe?g|webp|gif|bmp)$/i;
/** A word as a path: without surrounding quotes or brackets, or the sentence punctuation after it. */
const asPath = (word: string) => word.replace(/^["'(<\[]+/, "").replace(/["')>\]?!,;:.]+$/, "");
function isImageFile(word: string): boolean {
  const path = asPath(word);
  try {
    return IMAGE_FILE.test(path) && existsSync(path) && statSync(path).isFile();
  } catch {
    return false;
  }
}
/** Every existing image path among the words of the prompt, in order, each once. */
function imagePathsIn(positionals: readonly string[]): string[] {
  const words = positionals.flatMap((arg) => arg.split(/\s+/)).filter(Boolean);
  return [...new Set(words.filter(isImageFile).map(asPath))];
}

/** Routing fields only: what `--preview` (route without running) accepts. */
const PREVIEW_FIELDS = new Set(["endpoint", "model", "job", "workflow", "task", "objective", "contextTokens", "sessionId", "minConfidence", "executionPreference", "preview"]);

const OPTIONS = {
  // ollama run
  format: { type: "string" },
  think: { type: "string" },
  hidethinking: { type: "boolean" },
  keepalive: { type: "string" },
  verbose: { type: "boolean" },
  nowordwrap: { type: "boolean" },
  insecure: { type: "boolean" },
  dimensions: { type: "string" },
  truncate: { type: "string" },
  voice: { type: "string" },
  ask: { type: "string", multiple: true },
  choice: { type: "string", multiple: true },
  level: { type: "string", multiple: true },
  // FreeLlama
  model: { type: "string", short: "m" },
  fallback: { type: "string", multiple: true },
  "idempotency-key": { type: "string" },
  job: { type: "string", short: "j" },
  workflow: { type: "string", short: "w" },
  task: { type: "string", short: "t" },
  objective: { type: "string" },
  file: { type: "string", short: "f", multiple: true },
  image: { type: "string", multiple: true },
  system: { type: "string", short: "s" },
  option: { type: "string", short: "o", multiple: true },
  ctx: { type: "string" },
  top: { type: "string" },
  tools: { type: "string" },
  session: { type: "string" },
  priority: { type: "string" },
  timeout: { type: "string" },
  "max-wait": { type: "string" },
  "min-confidence": { type: "string" },
  prefer: { type: "string" },
  defer: { type: "boolean" },
  preview: { type: "boolean" },
  "save-as": { type: "string" },
  schema: { type: "string" },
  suffix: { type: "string" },
  extract: { type: "boolean", short: "x" },
  json: { type: "boolean" },
  each: { type: "boolean" },
  stream: { type: "boolean" },
  "no-stream": { type: "boolean" },
  quiet: { type: "boolean", short: "q" },
  endpoint: { type: "string" },
  help: { type: "boolean", short: "h" },
} satisfies ParseArgsConfig["options"];

/** Ollama's `--think` and `--truncate` take an optional value (`--think=high`); bare means true. */
const OPTIONAL_VALUE = new Set(["--think", "--truncate"]);

/** Read `@path` as the file's text; anything else is the value itself. */
export function valueOrFile(value: string, flag: string): string {
  if (!value.startsWith("@")) return value;
  try {
    return readFileSync(value.slice(1), "utf8");
  } catch (error) {
    throw new UsageError(`${flag} ${value}: ${(error as Error).message}`);
  }
}

function jsonValue(value: string, flag: string): unknown {
  const text = valueOrFile(value, flag);
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new UsageError(`${flag} is not valid JSON: ${(error as Error).message}`);
  }
}

function positiveInt(value: string | undefined, flag: string): number | undefined {
  if (value === undefined) return undefined;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) throw new UsageError(`${flag} needs a positive integer, got "${value}".`);
  return parsed;
}

/** `k=v` with v read as a number or boolean when it is one: Ollama options are typed. */
function optionPair(pair: string): [string, unknown] {
  const at = pair.indexOf("=");
  if (at <= 0) throw new UsageError(`--option needs key=value, got "${pair}".`);
  const key = pair.slice(0, at);
  const raw = pair.slice(at + 1);
  if (raw === "true" || raw === "false") return [key, raw === "true"];
  const number = Number(raw);
  return [key, raw.trim() !== "" && Number.isFinite(number) ? number : raw];
}

/** true, false, or a level the model names (`ollama show MODEL`: low, medium, high, xhigh…); serve checks it against the model. */
function thinkValue(value: string | undefined): boolean | string | undefined {
  if (value === undefined) return undefined;
  if (value === "true" || value === "false") return value === "true";
  if (!/^[a-z][a-z0-9_-]{0,15}$/.test(value)) throw new UsageError(`--think takes true, false or a level the model lists (ollama show MODEL), got "${value}".`);
  return value;
}

const SCHEMA_TYPES: Record<string, Record<string, unknown>> = {
  str: { type: "string" }, string: { type: "string" }, int: { type: "integer" }, integer: { type: "integer" },
  float: { type: "number" }, number: { type: "number" }, bool: { type: "boolean" }, boolean: { type: "boolean" },
  list: { type: "array", items: { type: "string" } },
};

/**
 * `--schema`: a JSON schema (`{...}` or `@file.json`), or the short form llm uses: fields split by
 * commas or newlines, each `name [str|int|float|bool|list][: description]`, all required.
 * `--schema 'name, age int, tags list: up to five'`.
 */
function schemaValue(value: string): Record<string, unknown> {
  const text = valueOrFile(value, "--schema").trim();
  if (text.startsWith("{")) {
    const parsed = jsonValue(text, "--schema");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new UsageError("--schema JSON must be an object schema.");
    return parsed as Record<string, unknown>;
  }
  const properties: Record<string, Record<string, unknown>> = {};
  for (const field of text.split(/[,\n]/).map((part) => part.trim()).filter(Boolean)) {
    const at = field.indexOf(":");
    const head = (at < 0 ? field : field.slice(0, at)).trim().split(/\s+/);
    const description = at < 0 ? undefined : field.slice(at + 1).trim();
    const [name, kind = "str", ...extra] = head;
    const type = SCHEMA_TYPES[kind.toLowerCase()];
    if (!name || !type || extra.length > 0) throw new UsageError(`--schema field "${field}": write \`name [str|int|float|bool|list][: description]\`.`);
    properties[name] = { ...type, ...(description ? { description } : {}) };
  }
  if (Object.keys(properties).length === 0) throw new UsageError("--schema names no fields.");
  return { type: "object", properties, required: Object.keys(properties) };
}

type Question = { type: "noul" | "choice" | "score"; instructions: string; criteria?: Record<string, string | null> | string[] };

/**
 * `--ask` questions in argv order, each followed by its `--choice k=desc` or `--level L` flags,
 * as Ollama reads them. Named q1, q2, … in the order asked.
 */
function questionsFrom(tokens: ReturnType<typeof parseArgs>["tokens"]): Record<string, Question> | undefined {
  const questions: Array<[string, Question]> = [];
  for (const token of tokens ?? []) {
    if (token.kind !== "option" || !["ask", "choice", "level"].includes(token.name)) continue;
    const value = token.value ?? "";
    if (token.name === "ask") {
      questions.push([questionName(value, questions.map(([name]) => name)), { type: "noul", instructions: value }]);
      continue;
    }
    const current = questions.at(-1)?.[1];
    if (!current) throw new UsageError(`--${token.name} must follow the --ask it belongs to.`);
    if (token.name === "choice") {
      if (current.type === "score") throw new UsageError("one --ask takes --choice or --level, not both.");
      const at = value.indexOf("=");
      const criteria = (current.criteria ?? {}) as Record<string, string | null>;
      if (at < 0) criteria[value] = null;
      else criteria[value.slice(0, at)] = value.slice(at + 1);
      current.type = "choice";
      current.criteria = criteria;
    } else {
      if (current.type === "choice") throw new UsageError("one --ask takes --choice or --level, not both.");
      current.type = "score";
      current.criteria = [...((current.criteria as string[] | undefined) ?? []), value];
    }
  }
  return questions.length > 0 ? Object.fromEntries(questions) : undefined;
}

/** A short answer label from the question's first words (`Is a refund requested?` -> `refund-requested`), unique among `taken`. */
function questionName(question: string, taken: string[]): string {
  const STOP = new Set(["is", "are", "the", "a", "an", "does", "do", "this", "it", "which", "what", "how", "of", "to", "for", "be"]);
  const base = question.toLowerCase().replace(/[^a-z0-9\s-]/g, " ").split(/\s+/).filter((word) => word && !STOP.has(word)).slice(0, 3).join("-") || "question";
  let name = base.slice(0, 40);
  for (let n = 2; taken.includes(name); n += 1) name = `${base.slice(0, 36)}-${n}`;
  return name;
}

/** The job a run belongs to: the named one, or the workflow's. */
function jobOf(job: string | undefined, workflow: string | undefined): JobName | undefined {
  if (workflow !== undefined) {
    const flow = (WORKFLOWS as Record<string, { job: JobName }>)[workflow];
    if (!flow) throw new UsageError(`unknown workflow "${workflow}"; \`freellama workflows\` lists them.`);
    return flow.job;
  }
  if (job === undefined) return undefined;
  if (!(RUN_TASK_JOBS as readonly string[]).includes(job)) throw new UsageError(`unknown job "${job}"; \`freellama workflows\` lists them.`);
  return job as JobName;
}

export const RUN_USAGE = `usage: freellama run [MODEL] [PROMPT...] [flags]

Runs one task through FreeLlama: with --job or --workflow it picks the tested model that is
installed and fits now and gives it that job's prompts and options. Piped stdin is the input.
With no prompt on a terminal it starts a chat.

ollama run flags: --format json|SCHEMA  --think[=true|false|high|medium|low]  --hidethinking
  --keepalive 5m  --verbose  --ask Q [--choice key=desc ...|--level L ...]  --nowordwrap
FreeLlama flags:
  -w, --workflow NAME   fix-bug, write-tests, review-code, summarize-log, extract-json ... (freellama workflows)
  -j, --job NAME        classify, find-code, duplicates, ocr, image-recognition, coding, summarize ...
  -m, --model TAG       exact installed tag (wins over the job's pick)
  --fallback TAG        try TAG when the model before it is refused before running: no room, input over its
                        context, not installed (repeatable, up to 4; never after a model started)
  -t, --task KIND       completion, coding, code-repair, tools, vision, embedding, long-context, decision
  -f, --file PATH|GLOB  files to read into the task (repeatable); --image PATH for images
  -s, --system TEXT|@F  system prompt     -o, --option k=v  Ollama option (repeatable)
  --ctx N  context window   --top N  matches for embedding jobs   --tools JSON|@F  tool definitions
  --objective fastest|balanced|quality   --prefer cpu|gpu   --min-confidence low|medium
  --session UUID  --priority interactive|normal|background  --timeout S  --max-wait S
  --schema 'name, age int, tags list'|JSON|@F  structured output   -x, --extract  print only the first code block
  --suffix TEXT|@F  fill in the middle: PROMPT is the code before the gap (models with the insert capability)
  --each  one answer per -f file, run as one batch   --defer  return a job id (freellama job ID)
  --idempotency-key KEY  with --defer on a serve with durable jobs: a resubmission returns the original job
  --preview  route only (payload flags are ignored)   --save-as NAME  write a handoff
  --json  full receipt on stdout   -q, --quiet  no stderr notes   --endpoint URL  serve endpoint
  --stream / --no-stream  tokens as they arrive (default on a terminal) / one managed answer (default in a pipe)

Answers go to stdout; check: lines (verify these) and notes go to stderr.`;

/**
 * Parse `run`/`chat` argv. `stdin` is the piped input, if any. The first positional is a model
 * only when nothing else names one and it has no whitespace (an Ollama tag cannot), so
 * `freellama run "Why is the sky blue?"` routes and `freellama run gemma4 "Why?"` pins.
 */
export function parseRunArgs(argv: string[], stdin?: string): RunPlan & { help: boolean } {
  const normalized = argv.map((arg) => (OPTIONAL_VALUE.has(arg) ? `${arg}=true` : arg));
  let parsed: ReturnType<typeof parseArgs<{ options: typeof OPTIONS; allowPositionals: true; tokens: true; strict: true; args: string[] }>>;
  try {
    parsed = parseArgs({ args: normalized, options: OPTIONS, allowPositionals: true, tokens: true, strict: true });
  } catch (error) {
    throw new UsageError((error as Error).message);
  }
  const { values, tokens } = parsed;
  const positionals = [...parsed.positionals];
  const render: RenderOptions = {
    json: values.json === true, verbose: values.verbose === true, quiet: values.quiet === true, hideThinking: values.hidethinking === true,
    ...(values.extract ? { extract: true } : {}),
    ...(values.stream ? { stream: true } : values["no-stream"] ? { stream: false } : {}),
  };
  if (values.stream && values["no-stream"]) throw new UsageError("--stream and --no-stream contradict each other.");
  if (values.help) return { args: {}, render, promptless: true, help: true };


  if (values.truncate !== undefined && values.truncate !== "true") throw new UsageError("--truncate=false is not supported by managed embeddings; use `ollama run MODEL --truncate=false`.");
  if (values.voice !== undefined) throw new UsageError("--voice is speech output; use `freellama tool audio '{\"action\":\"speak\",...}'` (freellama tool audio --help).");

  const questions = questionsFrom(tokens);
  // `ollama run nimble --ask …`: the first word is still the model. With no model, a question
  // runs on the classify job's tested pick that fits now, not on whatever routing ranks first.
  const job = jobOf(values.job, values.workflow);
  const named = values.model !== undefined || job !== undefined || values.task !== undefined;
  let model = values.model;
  let positionalModel = false;
  if (!named && positionals.length > 0 && !/\s/.test(positionals[0]!) && !isImageFile(positionals[0]!)) {
    model = positionals.shift();
    positionalModel = true;
  }
  // Image paths in the prompt become attached images, as in `ollama run`.
  const promptImages = imagePathsIn(positionals);
  const prompt = positionals.length > 0 ? positionals.join(" ") : undefined;
  const input = stdin !== undefined && stdin.trim() !== "" ? stdin : undefined;

  // The classify job names its own task; an explicit one must still be "decision".
  // `freellama` documents code-repair and long-context; the task schema names them with underscores.
  const taskFlag = values.task?.replace(/-/g, "_");
  const task = questions !== undefined && (model !== undefined || taskFlag !== undefined) ? (taskFlag ?? "decision") : taskFlag;
  if (questions !== undefined && taskFlag !== undefined && taskFlag !== "decision") throw new UsageError(`--ask makes a decision task; remove --task ${task}.`);
  const kind = task ?? (job ? LOCAL_JOBS[job].task : questions !== undefined ? "decision" : undefined);

  const options: Record<string, unknown> = {};
  let contextTokens = positiveInt(values.ctx, "--ctx");
  for (const pair of values.option ?? []) {
    const [key, value] = optionPair(pair);
    // Context size is routing-owned; the flag that sets it is --ctx.
    if (key === "num_ctx") contextTokens = positiveInt(String(value), "--option num_ctx");
    else options[key] = value;
  }

  const files = [...new Set([...(values.file ?? []), ...(values.image ?? []), ...promptImages])];
  if (values.schema !== undefined && values.format !== undefined) throw new UsageError("--schema sets the output format; remove --format.");
  const format = values.schema !== undefined ? schemaValue(values.schema)
    : values.format === undefined ? undefined : values.format === "json" ? "json" : jsonValue(values.format, "--format");
  const args: Record<string, unknown> = {
    endpoint: values.endpoint,
    model,
    fallbackModels: values.fallback?.length ? values.fallback : undefined,
    job: values.workflow === undefined ? job ?? (questions !== undefined && model === undefined && values.task === undefined ? "classify" : undefined) : undefined,
    workflow: values.workflow as WorkflowName | undefined,
    task,
    objective: values.objective,
    files: files.length > 0 ? files : undefined,
    systemPrompt: values.system === undefined ? undefined : valueOrFile(values.system, "--system"),
    contextTokens,
    options: Object.keys(options).length > 0 ? options : undefined,
    format,
    think: thinkValue(values.think),
    keepAlive: values.keepalive,
    tools: values.tools === undefined ? undefined : jsonValue(values.tools, "--tools"),
    sessionId: values.session,
    priority: values.priority,
    timeoutSeconds: positiveInt(values.timeout, "--timeout"),
    maxWaitSeconds: positiveInt(values["max-wait"], "--max-wait"),
    minConfidence: values["min-confidence"],
    executionPreference: values.prefer === undefined ? undefined : values.prefer === "cpu" ? "prefer_cpu" : values.prefer === "gpu" ? "prefer_gpu" : values.prefer,
    defer: values.defer,
    idempotencyKey: values["idempotency-key"],
    preview: values.preview,
    saveAs: values["save-as"],
    dimensions: positiveInt(values.dimensions, "--dimensions"),
    // Fill in the middle: the prompt is the code before the gap, this is the code after it.
    suffix: values.suffix === undefined ? undefined : valueOrFile(values.suffix, "--suffix"),
  };
  if (args.suffix !== undefined && (job !== undefined || values.workflow !== undefined || questions !== undefined)) {
    throw new UsageError("--suffix fills in the middle on one model: name it (freellama run MODEL PREFIX --suffix SUFFIX), without -w, -j or --ask.");
  }

  if (kind === "decision") {
    // The question is about the input: stdin, a file, or the prompt words.
    args.questions = questions;
    const state = [prompt, input].filter((part) => part !== undefined).join("\n\n");
    if (state !== "") args.state = state;
  } else if (kind === "embedding") {
    // The prompt is what to look for among the inputs; stdin lines are inputs too.
    const lines = input?.split("\n").map((line) => line.trim()).filter(Boolean);
    if (lines?.length) args.input = lines;
    const top = positiveInt(values.top, "--top");
    if (prompt !== undefined || top !== undefined) args.nearest = { query: prompt, top };
  } else {
    const text = [prompt, input].filter((part) => part !== undefined).join("\n\n");
    if (text !== "") args.prompt = text;
  }
  if (values.dimensions !== undefined && kind !== undefined && kind !== "embedding") throw new UsageError("--dimensions shortens embedding vectors; use it with an embedding model or job.");
  if (values.top !== undefined && kind !== "embedding") throw new UsageError("--top ranks embedding matches; use it with an embedding job (find-code, duplicates, image-search).");
  const promptless = prompt === undefined && input === undefined && files.length === 0 && questions === undefined;
  if (values.each && files.length === 0) throw new UsageError("--each runs one task per file; name the files with -f PATH|GLOB.");
  if (values["idempotency-key"] !== undefined && !values.defer) throw new UsageError("--idempotency-key deduplicates deferred jobs; add --defer.");
  if (values["idempotency-key"] !== undefined && values.each) throw new UsageError("--idempotency-key names one job; it does not apply to --each.");
  // A preview routes without running: payload fields (prompt, files, options…) do not apply to it.
  const routed = values.preview ? Object.fromEntries(Object.entries(args).filter(([name]) => PREVIEW_FIELDS.has(name))) : args;
  return { args: withoutUndefined(routed), render, promptless, help: false, ...(values.each ? { each: true } : {}), ...(positionalModel ? { positionalModel: true } : {}) };
}
