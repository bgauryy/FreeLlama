// `freellama`: Ollama-style commands that run through FreeLlama. `run` is one managed task with job
// mode's picks and prompts; `chat` is the real `ollama run` REPL pointed at an in-process gateway
// that serves the workflows as virtual models; `gateway` keeps that listener up for any Ollama or
// OpenAI client; `tool` calls any MCP tool; Ollama's own commands pass through to `ollama`.
import { spawn } from "node:child_process";
import { fstatSync, readFileSync } from "node:fs";
import { LOCAL_JOBS, RUN_TASK_JOBS, WORKFLOWS, WORKFLOW_NAMES, type JobName } from "../local-prompts.js";
import { configureServeAutostart } from "../serve.js";
import { SERVER_VERSION } from "../native.js";
import { parseRunArgs, RUN_USAGE, UsageError, valueOrFile } from "./args.js";
import { callTool, resultText, toolRegistry } from "./registry.js";
import { EXIT_RETRY, renderResult } from "./output.js";
import { OLLAMA_MISSING, OLLAMA_STOPPED, ollamaPresence } from "../ollama-presence.js";
import { expandFileEntries } from "../task-files.js";
import { installedInventory } from "../model-library.js";
import type { InstalledModel } from "../model-catalog.js";
import { streamRun, streamTarget } from "./stream.js";
import { startGateway } from "./gateway.js";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { CHAT_JOB, VIRTUAL_PREFIX, virtualModels } from "./virtual.js";
import { pickLiveJobModel } from "../job-mode.js";

/** Ollama's own commands: `freellama list` is `ollama list`, byte for byte. */
const USAGE = `freellama agent commands: run local models with FreeLlama's tested picks, prompts and memory gate.

  freellama run [MODEL] [PROMPT...] [flags]   one task; ollama run flags plus -w/--workflow, -j/--job, -f/--file ...
  freellama chat [NAME] [ollama run flags]    the ollama chat on a workflow (review-code) or a tag
  freellama gateway [--port 11437]            serve workflows as models to Ollama, OpenAI and Anthropic clients
  freellama workflows                         workflows and jobs, with what each expects
  freellama status [--endpoint URL]           on a terminal: memory, queues, loaded models, active jobs
  freellama job ID [--wait|--cancel] [--endpoint URL]   a deferred task's result (--wait: block until it ends)
  freellama tool NAME [JSON|@FILE|-]          call one FreeLlama MCP tool; freellama tools lists them

Examples:
  git diff | freellama run -w summarize-diff
  freellama run -w fix-bug -f src/parser.ts "The parser drops the last token"
  freellama run -j find-code -f 'src/**/*.ts' "where are CLI flags parsed"
  freellama run --ask "Is a refund requested?" < ticket.txt
  freellama chat review-code
  OLLAMA_HOST=127.0.0.1:11437 ollama run freellama/explain-code     (with freellama gateway running)

stdout is the answer; check: lines (verify these first) and notes go to stderr. Exit 0 ok, 75 busy or
short of memory (retry later), 1 refused or failed, 2 usage, 130 interrupted. freellama --help lists the
operator commands (serve, doctor, catalog, task...).`;

type Io = { out: (text: string) => void; err: (text: string) => void; write?: (text: string) => void };
const stdio: Io = {
  out: (text) => process.stdout.write(text.endsWith("\n") ? text : `${text}\n`),
  err: (text) => process.stderr.write(text.endsWith("\n") ? text : `${text}\n`),
  write: (text) => process.stdout.write(text),
};

/**
 * Piped (`a | freellama run`), redirected (`< file`) or programmatic input. A FIFO or a file is read
 * to its end. A socket (how Node, and many agent harnesses, wire a child's stdin) is read only if
 * data or end-of-input arrives within STDIN_PROBE_MS: a program that passes input sends it at
 * once, while a harness's open socket that never ends is treated as no input instead of hanging
 * the call the way `ollama run` does.
 */
const STDIN_PROBE_MS = 250;
async function readStdin(): Promise<string | undefined> {
  let stat;
  try {
    stat = fstatSync(0);
  } catch {
    return undefined;
  }
  if (stat.isFIFO() || stat.isFile()) return readFileSync(0, "utf8");
  if (!stat.isSocket()) return undefined;
  return new Promise((resolve) => {
    const chunks: Buffer[] = [];
    const done = (text: string | undefined) => {
      clearTimeout(timer);
      process.stdin.removeAllListeners("data").removeAllListeners("end").removeAllListeners("error");
      resolve(text);
    };
    const timer = setTimeout(() => {
      if (chunks.length > 0) return;
      process.stdin.destroy();
      done(undefined);
    }, STDIN_PROBE_MS);
    process.stdin.on("data", (chunk: Buffer) => chunks.push(chunk));
    process.stdin.on("end", () => done(chunks.length > 0 ? Buffer.concat(chunks).toString("utf8") : undefined));
    process.stdin.on("error", () => done(undefined));
  });
}

/** Ctrl-C aborts the call (the handler cancels its job); a second one exits at once. */
function interruptSignal(): AbortSignal {
  const controller = new AbortController();
  process.once("SIGINT", () => {
    controller.abort();
    process.once("SIGINT", () => process.exit(130));
  });
  return controller.signal;
}

function progressPrinter(io: Io, verbose: boolean, quiet: boolean) {
  if (quiet) return undefined;
  const show = verbose || process.stderr.isTTY === true;
  return (level: "progress" | "info" | "warning", message: string) => {
    if (level === "warning" || show) io.err(`freellama: ${message}`);
  };
}

async function run(argv: string[], io: Io): Promise<number> {
  const stdin = await readStdin();
  const plan = parseRunArgs(argv, stdin);
  if (plan.help) {
    io.out(RUN_USAGE);
    return 0;
  }
  const stop = await ollamaReady(io, plan.args.endpoint);
  if (stop !== undefined) return stop;
  // `ollama run MODEL` with no prompt on a terminal is a chat; so is this.
  if (plan.promptless && process.stdin.isTTY) {
    const name = (plan.args.workflow ?? plan.args.job ?? plan.args.model) as string | undefined;
    return chat(name === undefined ? [] : [name], io);
  }
  const signal = interruptSignal();
  const { args } = plan;
  // `freellama run WORD …`: check the word against the installed tags, as `ollama run` would.
  if (plan.positionalModel && typeof args.model === "string") {
    const settled = await settleModelWord(args, args.model);
    if (settled) return settled(io);
  }
  if (plan.each) return runEach(args, plan, io, signal);
  // On a terminal, tokens stream as `ollama run` shows them; in a pipe one managed answer comes back.
  // Fill in the middle always runs on the forwarded path (managed tasks are chats).
  const wantStream = args.suffix !== undefined || (plan.render.stream ?? (process.stdout.isTTY === true && !plan.render.json && !plan.render.extract));
  const target = wantStream && !plan.render.json && !plan.render.extract ? streamTarget(args) : undefined;
  if (target) {
    return streamRun(target, args, plan.render, { ...io, write: io.write ?? ((text) => io.out(text)) }, signal, process.stderr.isTTY === true);
  }
  // A bare prompt runs on the tested chat pick that fits now; routing alone ignores free memory.
  if (["model", "job", "workflow", "task", "preview"].every((field) => args[field] === undefined) && typeof args.prompt === "string") {
    args.model = await pickLiveJobModel(CHAT_JOB, args.endpoint as string | undefined).catch(() => undefined);
  }
  const result = await callTool("run_task", args, { signal, onMessage: progressPrinter(io, plan.render.verbose, plan.render.quiet) });
  if (signal.aborted) {
    io.err(resultText(result) || "freellama: cancelled");
    return 130;
  }
  const rendered = renderResult(result, plan.render, process.stderr.isTTY === true);
  for (const line of rendered.stderr) io.err(line);
  if (rendered.stdout !== "") io.out(rendered.stdout);
  return rendered.exitCode;
}

/**
 * A first word taken as a model: an installed tag stays one (an embedding-only tag embeds its
 * prompt and prints the vector, as `ollama run` does); a word that is no tag and has no prompt after
 * it is the prompt; an unknown tag before a prompt is a usage error naming the nearest tags.
 * Returns a printer when the run is decided here.
 */
async function settleModelWord(args: Record<string, unknown>, word: string): Promise<((io: Io) => number) | undefined> {
  let installed: InstalledModel[];
  try {
    installed = await installedInventory(undefined);
  } catch {
    return undefined; // Inventory unreadable: let the call report what is down.
  }
  const lower = word.toLowerCase();
  const match = installed.find((model) => model.name.toLowerCase() === lower || model.name.toLowerCase() === `${lower}:latest`);
  if (match) {
    args.model = match.name;
    const caps = match.capabilities ?? [];
    if (caps.includes("embedding") && !caps.includes("completion") && args.task === undefined && typeof args.prompt === "string") {
      args.task = "embedding";
      args.input = args.prompt;
      args.returnEmbeddings = true;
      delete args.prompt;
    }
    return undefined;
  }
  if (args.prompt === undefined && !word.includes(":") && !word.includes("/")) {
    // `freellama run hello`: no tag of that name, nothing after it; it is the prompt.
    delete args.model;
    args.prompt = word;
    return undefined;
  }
  // The tag the word starts (lfm3:1.7b -> lfm3:1.7b-q4_K_M) first, then the same family.
  const family = lower.split(":")[0]!;
  const rank = (name: string) => (name.toLowerCase().startsWith(lower) ? 0 : name.toLowerCase().startsWith(`${family}:`) ? 1 : 2);
  const near = installed.map((model) => model.name)
    .filter((name) => name.toLowerCase().startsWith(family) || name.toLowerCase().includes(lower))
    .sort((a, b) => rank(a) - rank(b)).slice(0, 3);
  throw new UsageError(`"${word}" is not an installed model${near.length ? `; did you mean ${near.join(", ")}?` : "; `freellama list` shows the installed tags"} (freellama never pulls; ask before \`ollama pull\`)`);
}

/** --each: one task per file (globs expanded), the same request on each, as one batch. */
async function runEach(args: Record<string, unknown>, plan: ReturnType<typeof parseRunArgs>, io: Io, signal: AbortSignal): Promise<number> {
  const paths = expandFileEntries(args.files as string[]);
  if (paths.length === 0) throw new UsageError("--each: no file matches the -f patterns.");
  if (paths.length > 64) throw new UsageError(`--each: ${paths.length} files; a batch takes at most 64 (narrow the glob, or use freellama tool sweep for a tree).`);
  const { files: _files, prompt, ...shared } = args;
  const tasks = paths.map((path) => ({ id: path, independent: true, task: { files: [path], ...(typeof prompt === "string" ? { prompt } : {}) } }));
  const result = await callTool("run_task", { ...shared, tasks }, { signal, onMessage: progressPrinter(io, plan.render.verbose, plan.render.quiet) });
  const rendered = renderResult(result, plan.render, false);
  for (const line of rendered.stderr) io.err(line);
  if (rendered.stdout !== "") io.out(rendered.stdout);
  return rendered.exitCode;
}

/**
 * Commands that need Ollama say at once when it is missing (install it, with the link) or stopped
 * (start it), instead of after serve's startup waits. Returns the exit code to stop with, or
 * undefined to go on. A serve named with --endpoint may use an Ollama elsewhere, so it is not probed.
 */
async function ollamaReady(io: Io, endpoint: unknown): Promise<number | undefined> {
  if (endpoint !== undefined) return undefined;
  const presence = await ollamaPresence();
  if (presence === "missing") {
    io.err(`freellama: ${OLLAMA_MISSING}`);
    return 1;
  }
  if (presence === "stopped") {
    io.err(`freellama: ${OLLAMA_STOPPED}`);
    return EXIT_RETRY;
  }
  return undefined;
}

/** `review-code` -> `freellama/review-code`; a tag stays a tag. */
export function chatModelName(name: string | undefined): string {
  if (name === undefined) return `${VIRTUAL_PREFIX}auto`;
  if (name.startsWith(VIRTUAL_PREFIX)) return name;
  return virtualModels().some((model) => model.name === `${VIRTUAL_PREFIX}${name}`) ? `${VIRTUAL_PREFIX}${name}` : name;
}

/** The ollama REPL against a gateway on an ephemeral loopback port, for as long as the REPL runs. */
async function chat(argv: string[], io: Io): Promise<number> {
  const stop = await ollamaReady(io, undefined);
  if (stop !== undefined) return stop;
  const name = argv[0] === undefined || argv[0].startsWith("-") ? undefined : argv[0];
  const flags = name === undefined ? argv : argv.slice(1);
  const gateway = await startGateway({ port: 0, checksInContent: true });
  const model = chatModelName(name);
  io.err(`freellama: chatting with ${model} (FreeLlama gateway ${gateway.url}); /bye to leave`);
  try {
    return await new Promise<number>((resolve) => {
      const child = spawn(process.env.FREELLAMA_OLLAMA_CLI ?? "ollama", ["run", model, ...flags], {
        stdio: "inherit",
        env: { ...process.env, OLLAMA_HOST: `127.0.0.1:${gateway.port}` },
      });
      // The REPL owns Ctrl-C; this process only waits for it.
      const ignore = () => undefined;
      process.on("SIGINT", ignore);
      child.on("exit", (code, signal) => {
        process.off("SIGINT", ignore);
        resolve(signal ? 130 : code ?? 0);
      });
      child.on("error", (error) => {
        io.err(`freellama: could not start the ollama CLI: ${error.message}. Install Ollama, or set FREELLAMA_OLLAMA_CLI.`);
        resolve(1);
      });
    });
  } finally {
    await gateway.close();
  }
}

async function gateway(argv: string[], io: Io): Promise<number> {
  let port = 11437;
  let serveEndpoint: string | undefined;
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--port") port = Number(argv[++index]);
    else if (flag === "--endpoint") serveEndpoint = argv[++index];
    else if (flag === "-h" || flag === "--help") {
      io.out("usage: freellama gateway [--port 11437] [--endpoint SERVE_URL]\nServes freellama/<workflow> virtual models on loopback over the Ollama (/api), OpenAI (/v1/chat/completions, /v1/responses) and Anthropic (/v1/messages) APIs; every other request goes to serve.");
      return 0;
    } else throw new UsageError(`unknown gateway flag "${flag}"`);
  }
  if (!Number.isInteger(port) || port < 0 || port > 65_535) throw new UsageError("--port needs a port number");
  const stop = await ollamaReady(io, serveEndpoint);
  if (stop !== undefined) return stop;
  const started = await startGateway({ port, serveEndpoint, log: (line) => io.err(`freellama gateway: ${line}`) });
  const hostPort = started.url.replace(/^http:\/\//, "");
  io.err(`freellama gateway on ${started.url}: ${virtualModels().length} virtual models; other requests go to serve.
  ollama:  OLLAMA_HOST=${hostPort} ollama run freellama/fix-bug
  openai:  base URL ${started.url}/v1, model "freellama/review-code"
  claude:  ANTHROPIC_BASE_URL=${started.url}  (Codex: wire_api = "responses", base_url ${started.url}/v1)
  list:    OLLAMA_HOST=${hostPort} ollama list`);
  await new Promise<void>((resolve) => {
    process.once("SIGINT", resolve);
    process.once("SIGTERM", resolve);
  });
  await started.close();
  return 0;
}

async function toolArgs(raw: string | undefined): Promise<Record<string, unknown>> {
  if (raw === undefined) return {};
  const text = raw === "-" ? (await readStdin()) ?? "" : valueOrFile(raw, "arguments");
  try {
    const parsed: unknown = JSON.parse(text);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) return parsed as Record<string, unknown>;
  } catch (error) {
    throw new UsageError(`tool arguments are not valid JSON: ${(error as Error).message}`);
  }
  throw new UsageError("tool arguments must be a JSON object");
}

async function tool(argv: string[], io: Io): Promise<number> {
  const json = argv.includes("--json");
  const [name, raw] = argv.filter((arg) => arg !== "--json");
  if (name === undefined) return tools(io);
  const registered = toolRegistry().get(name);
  if (raw === "--help" || raw === "-h") {
    if (!registered) throw new UsageError(`unknown tool "${name}"; \`freellama tools\` lists them.`);
    io.out(`${name}: ${registered.description}\n\ninput schema:\n${JSON.stringify(registered.inputSchema.toJSONSchema?.() ?? {}, null, 2)}`);
    return 0;
  }
  const signal = interruptSignal();
  const args = await toolArgs(raw);
  let result = await callTool(name, args, { signal, onMessage: progressPrinter(io, false, false) });
  // A sweep runs in the process that started it: a one-shot command follows it to the end, or
  // exiting would interrupt it.
  const sweepId = result.structuredContent?.id;
  if (name === "sweep" && (args.action === "start" || args.action === "resume") && !result.isError && typeof sweepId === "string") {
    while (!signal.aborted && ["running", "collecting"].includes(String(result.structuredContent?.phase))) {
      result = await callTool("sweep", { action: "wait", sweepId }, { signal, onMessage: progressPrinter(io, true, false) });
      if (result.isError) break;
    }
  }
  const rendered = renderResult(result, { json, verbose: false, quiet: false, hideThinking: true });
  for (const line of rendered.stderr) io.err(line);
  if (rendered.stdout !== "") io.out(rendered.stdout);
  return rendered.exitCode;
}

const gb = (bytes: unknown) => (typeof bytes === "number" ? `${(bytes / 1e9).toFixed(1)} GB` : "?");

/** Time left before a runner unloads, from `/api/ps` `expires_at` (a far-future date means pinned). */
function until(expires: unknown, now = Date.now()): string {
  const at = typeof expires === "string" ? Date.parse(expires) : NaN;
  if (Number.isNaN(at)) return "?";
  const seconds = Math.round((at - now) / 1000);
  if (seconds > 365 * 86_400) return "pinned";
  return seconds <= 0 ? "now" : seconds < 120 ? `${seconds} s` : `${Math.round(seconds / 60)} min`;
}

/** `freellama status`: serve, the host, queues and usage, then each loaded runner and the active jobs. */
async function status(argv: string[], io: Io): Promise<number> {
  const at = argv.indexOf("--endpoint");
  const endpoint = at >= 0 ? argv[at + 1] : undefined;
  const stop = await ollamaReady(io, endpoint);
  if (stop !== undefined) return stop;
  const result = await callTool("doctor", { view: "status", ...(endpoint ? { endpoint } : {}) });
  if (argv.includes("--json")) {
    io.out(JSON.stringify(result.structuredContent ?? {}, null, 2));
    return result.isError ? 1 : 0;
  }
  const rendered = renderResult(result, { json: false, verbose: false, quiet: false, hideThinking: true }, false);
  if (result.isError) {
    for (const line of rendered.stderr) io.err(line);
    return rendered.exitCode;
  }
  io.out(resultText(result).split("\n").filter((line) => !line.startsWith("Full object")).join("\n"));
  // The runners Ollama holds now, as `ollama ps` shows them, through serve.
  try {
    const reply = await fetch(new URL("/api/ps", endpoint ?? DEFAULT_SERVE_ENDPOINT), { signal: AbortSignal.timeout(3_000) });
    const models = ((await reply.json()) as { models?: Array<Record<string, unknown>> }).models ?? [];
    if (models.length > 0) {
      io.out("\nMODEL                                   SIZE      ON     WINDOW   UNLOADS");
      for (const model of models) {
        const onGpu = typeof model.size_vram === "number" && typeof model.size === "number" && model.size > 0 ? Math.round((model.size_vram / model.size) * 100) : undefined;
        io.out(`${String(model.name).padEnd(40)}${gb(model.size).padEnd(10)}${(onGpu === undefined ? "?" : onGpu === 100 ? "GPU" : onGpu === 0 ? "CPU" : `${onGpu}% GPU`).padEnd(7)}${String(model.context_length ?? "?").padEnd(9)}${until(model.expires_at)}`);
      }
    }
  } catch {
    // Loaded runners are optional detail; the status above stands.
  }
  const jobs = (result.structuredContent?.task_jobs as { jobs?: Array<Record<string, unknown>> } | undefined)?.jobs ?? [];
  if (jobs.length > 0) {
    io.out("\nACTIVE JOBS");
    for (const job of jobs) io.out(`${String(job.id).slice(0, 8)}  ${String(job.task ?? "?").padEnd(11)}${String(job.selected_model ?? job.requested_model ?? "-").padEnd(28)}${String(job.status)}${job.reason ? ` (${String(job.reason)})` : ""}`);
  }
  return 0;
}

/** `freellama job ID --wait`: poll until the job ends, then print only its answer (checks on stderr). */
async function waitJob(id: string, io: Io, endpoint?: string): Promise<number> {
  const signal = interruptSignal();
  for (let delay = 100; !signal.aborted; delay = Math.min(2_000, delay * 2)) {
    const result = await callTool("task_jobs", { action: "get", jobId: id, ...(endpoint ? { endpoint } : {}) }, { signal });
    const job = result.structuredContent?.job as Record<string, unknown> | undefined;
    if (result.isError || ["completed", "failed", "cancelled", "expired"].includes(String(job?.status))) {
      // The job's line comes first in the text; the answer follows it.
      const text = (result.content ?? []).map((part) => part.text ?? "").join("\n").split("\n").slice(1).join("\n");
      const rendered = renderResult({ ...result, content: [{ type: "text", text }] }, { json: false, verbose: false, quiet: false, hideThinking: true }, false);
      for (const line of rendered.stderr) io.err(line);
      if (rendered.stdout !== "") io.out(rendered.stdout);
      return job?.status === "completed" ? rendered.exitCode : 1;
    }
    await new Promise((resolve) => setTimeout(resolve, delay));
  }
  return 130;
}

function tools(io: Io): number {
  for (const registered of toolRegistry().values()) {
    io.out(`${registered.name.padEnd(18)} ${registered.description.split(/(?<=\.) /)[0]}`);
  }
  io.out("\nfreellama tool NAME --help shows a tool's input schema.");
  return 0;
}

function workflows(io: Io): number {
  io.out("Workflows (freellama run -w NAME; chat and gateway: freellama/NAME):");
  for (const name of WORKFLOW_NAMES) {
    const flow = WORKFLOWS[name] as { job: JobName; title: string; input: string };
    io.out(`  ${name.padEnd(16)} ${flow.title} [job ${flow.job}; ${flow.input}]`);
  }
  io.out("\nJobs (freellama run -j NAME):");
  for (const job of RUN_TASK_JOBS) {
    const spec = LOCAL_JOBS[job];
    io.out(`  ${job.padEnd(18)} ${spec.task.padEnd(9)} ${spec.notes.split(/(?<=\.) /)[0]}`);
  }
  return 0;
}

export async function main(argv: string[], io: Io = stdio): Promise<number> {
  const [command, ...rest] = argv;
  try {
    if (command === undefined || command === "help" || command === "-h" || command === "--help") {
      io.out(USAGE);
      return 0;
    }
    if (command === "-v" || command === "--version") {
      io.out(`freellama ${SERVER_VERSION}`);
      return 0;
    }
    // A one-shot command must not stop serve on exit: start it detached, logging to a file.
    configureServeAutostart({
      detach: true,
      onStarted: (pid, logFile) => io.err(`freellama: started freellama serve in the background (pid ${pid}, log ${logFile}); stop it with \`kill ${pid}\``),
    });
    switch (command) {
      case "run": return await run(rest, io);
      case "chat": return await chat(rest, io);
      case "gateway": return await gateway(rest, io);
      case "tool": return await tool(rest, io);
      case "tools": return tools(io);
      case "workflows": return workflows(io);
      case "status": return await status(rest, io);
      case "job": {
        // A job lives on the serve that ran it: --endpoint names that serve, as on `freellama run`.
        const at = rest.indexOf("--endpoint");
        const endpoint = at >= 0 ? rest[at + 1] : undefined;
        const words = at >= 0 ? rest.filter((_, index) => index !== at && index !== at + 1) : rest;
        const [id, flag] = words;
        const where = endpoint ? { endpoint } : {};
        if (id === undefined) return await tool(["task_jobs", JSON.stringify({ action: "list", ...where })], io);
        if (flag === "--wait") return await waitJob(id, io, endpoint);
        return await tool(["task_jobs", JSON.stringify({ action: flag === "--cancel" ? "cancel" : "get", jobId: id, ...where })], io);
      }
      default:
        throw new UsageError(`unknown command "${command}"; freellama --help lists them.`);
    }
  } catch (error) {
    if (error instanceof UsageError) {
      io.err(`freellama: ${error.message}`);
      return 2;
    }
    io.err(`freellama: ${(error as Error).message}`);
    return 1;
  }
}
