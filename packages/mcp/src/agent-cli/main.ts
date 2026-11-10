// `fllama`: Ollama-style commands that run through FreeLlama. `run` is one managed task with job
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
import { renderResult } from "./output.js";
import { startGateway, VIRTUAL_PREFIX, virtualModels } from "./gateway.js";

/** Ollama's own commands: `fllama list` is `ollama list`, byte for byte. */
export const OLLAMA_COMMANDS = new Set(["serve", "create", "show", "stop", "pull", "push", "signin", "signout", "list", "ls", "ps", "cp", "rm", "launch"]);

export const USAGE = `fllama: run local Ollama models with FreeLlama's tested picks, prompts and admission.

  fllama run [MODEL] [PROMPT...] [flags]   one task; ollama run flags plus -w/--workflow, -j/--job, -f/--file ...
  fllama chat [NAME] [ollama run flags]    the ollama REPL on a FreeLlama virtual model (freellama/fix-bug) or a tag
  fllama gateway [--port 11437]            serve virtual models over the Ollama and OpenAI APIs
  fllama workflows                         workflows and jobs, with what each expects
  fllama catalog [JOB]                     tested picks per job: installed, fits now
  fllama job ID [--cancel]                 a deferred task's result
  fllama tool NAME [JSON|@FILE|-]          call one FreeLlama MCP tool; fllama tools lists them
  fllama list|ps|show|pull|rm|cp|create|stop|push|serve ...   passed to ollama unchanged

Examples:
  git diff | fllama run -w summarize-diff
  fllama run -w fix-bug -f src/parser.ts "The parser drops the last token"
  fllama run -j find-code -f 'src/**/*.ts' "where are CLI flags parsed"
  fllama run --ask "Is a refund requested?" < ticket.txt
  fllama chat review-code
  OLLAMA_HOST=127.0.0.1:11437 ollama run freellama/explain-code     (with fllama gateway running)

stdout is the answer; check: lines (verify these first) and notes go to stderr. Exit 0 ok, 1 refused
or failed, 2 usage, 130 interrupted. A pull, rm or create still needs the user's approval.`;

type Io = { out: (text: string) => void; err: (text: string) => void };
const stdio: Io = {
  out: (text) => process.stdout.write(text.endsWith("\n") ? text : `${text}\n`),
  err: (text) => process.stderr.write(text.endsWith("\n") ? text : `${text}\n`),
};

/**
 * Piped (`a | fllama run`) or redirected (`< file`) input. Only a FIFO or a regular file is read:
 * agent harnesses often hand a command an open socket that never reaches EOF, where `ollama run`
 * waits forever.
 */
function readStdin(): string | undefined {
  try {
    const stat = fstatSync(0);
    if (!stat.isFIFO() && !stat.isFile()) return undefined;
    return readFileSync(0, "utf8");
  } catch {
    return undefined;
  }
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
    if (level === "warning" || show) io.err(`fllama: ${message}`);
  };
}

async function run(argv: string[], io: Io): Promise<number> {
  const stdin = readStdin();
  const plan = parseRunArgs(argv, stdin);
  if (plan.help) {
    io.out(RUN_USAGE);
    return 0;
  }
  // `ollama run MODEL` with no prompt on a terminal is a chat; so is this.
  if (plan.promptless && process.stdin.isTTY) {
    const name = (plan.args.workflow ?? plan.args.job ?? plan.args.model) as string | undefined;
    return chat(name === undefined ? [] : [name], io);
  }
  const signal = interruptSignal();
  const result = await callTool("run_task", plan.args, { signal, onMessage: progressPrinter(io, plan.render.verbose, plan.render.quiet) });
  if (signal.aborted) {
    io.err(resultText(result) || "fllama: cancelled");
    return 130;
  }
  const rendered = renderResult(result, plan.render, process.stderr.isTTY === true);
  for (const line of rendered.stderr) io.err(line);
  if (rendered.stdout !== "") io.out(rendered.stdout);
  return rendered.exitCode;
}

/** `review-code` -> `freellama/review-code`; a tag stays a tag. */
export function chatModelName(name: string | undefined): string {
  if (name === undefined) return `${VIRTUAL_PREFIX}auto`;
  if (name.startsWith(VIRTUAL_PREFIX)) return name;
  return virtualModels().some((model) => model.name === `${VIRTUAL_PREFIX}${name}`) ? `${VIRTUAL_PREFIX}${name}` : name;
}

/** The ollama REPL against a gateway on an ephemeral loopback port, for as long as the REPL runs. */
async function chat(argv: string[], io: Io): Promise<number> {
  const name = argv[0] === undefined || argv[0].startsWith("-") ? undefined : argv[0];
  const flags = name === undefined ? argv : argv.slice(1);
  const gateway = await startGateway({ port: 0, checksInContent: true });
  const model = chatModelName(name);
  io.err(`fllama: chatting with ${model} (FreeLlama gateway ${gateway.url}); /bye to leave`);
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
        io.err(`fllama: could not start the ollama CLI: ${error.message}. Install Ollama, or set FREELLAMA_OLLAMA_CLI.`);
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
      io.out("usage: fllama gateway [--port 11437] [--endpoint SERVE_URL]\nServes freellama/<workflow> virtual models over the Ollama (/api) and OpenAI (/v1) APIs on loopback; every other request goes to serve.");
      return 0;
    } else throw new UsageError(`unknown gateway flag "${flag}"`);
  }
  if (!Number.isInteger(port) || port < 0 || port > 65_535) throw new UsageError("--port needs a port number");
  const started = await startGateway({ port, serveEndpoint, log: (line) => io.err(`fllama gateway: ${line}`) });
  const hostPort = started.url.replace(/^http:\/\//, "");
  io.err(`fllama gateway on ${started.url}: ${virtualModels().length} virtual models; other requests go to serve.
  ollama:  OLLAMA_HOST=${hostPort} ollama run freellama/fix-bug
  openai:  base URL ${started.url}/v1, model "freellama/review-code"
  list:    OLLAMA_HOST=${hostPort} ollama list`);
  await new Promise<void>((resolve) => {
    process.once("SIGINT", resolve);
    process.once("SIGTERM", resolve);
  });
  await started.close();
  return 0;
}

function toolArgs(raw: string | undefined): Record<string, unknown> {
  if (raw === undefined) return {};
  const text = raw === "-" ? readFileSync(0, "utf8") : valueOrFile(raw, "arguments");
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
    if (!registered) throw new UsageError(`unknown tool "${name}"; \`fllama tools\` lists them.`);
    io.out(`${name}: ${registered.description}\n\ninput schema:\n${JSON.stringify(registered.inputSchema.toJSONSchema?.() ?? {}, null, 2)}`);
    return 0;
  }
  const result = await callTool(name, toolArgs(raw), { signal: interruptSignal(), onMessage: progressPrinter(io, false, false) });
  const rendered = renderResult(result, { json, verbose: false, quiet: false, hideThinking: true });
  for (const line of rendered.stderr) io.err(line);
  if (rendered.stdout !== "") io.out(rendered.stdout);
  return rendered.exitCode;
}

function tools(io: Io): number {
  for (const registered of toolRegistry().values()) {
    io.out(`${registered.name.padEnd(18)} ${registered.description.split(/(?<=\.) /)[0]}`);
  }
  io.out("\nfllama tool NAME --help shows a tool's input schema.");
  return 0;
}

function workflows(io: Io): number {
  io.out("Workflows (fllama run -w NAME; chat and gateway: freellama/NAME):");
  for (const name of WORKFLOW_NAMES) {
    const flow = WORKFLOWS[name] as { job: JobName; title: string; input: string };
    io.out(`  ${name.padEnd(16)} ${flow.title} [job ${flow.job}; ${flow.input}]`);
  }
  io.out("\nJobs (fllama run -j NAME):");
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
      io.out(`fllama ${SERVER_VERSION}`);
      return 0;
    }
    if (OLLAMA_COMMANDS.has(command)) return passthrough(argv, io);
    // A one-shot command must not stop serve on exit: start it detached, logging to a file.
    configureServeAutostart({
      detach: true,
      onStarted: (pid, logFile) => io.err(`fllama: started freellama serve in the background (pid ${pid}, log ${logFile}); stop it with \`kill ${pid}\``),
    });
    switch (command) {
      case "run": return await run(rest, io);
      case "chat": return await chat(rest, io);
      case "gateway": return await gateway(rest, io);
      case "tool": return await tool(rest, io);
      case "tools": return tools(io);
      case "workflows": return workflows(io);
      case "catalog": return await tool(["models", JSON.stringify({ view: "catalog", ...(rest[0] ? { job: rest[0] } : {}) })], io);
      case "job": {
        const [id, flag] = rest;
        if (id === undefined) return await tool(["task_jobs", JSON.stringify({ action: "list" })], io);
        return await tool(["task_jobs", JSON.stringify({ action: flag === "--cancel" ? "cancel" : "get", jobId: id })], io);
      }
      default:
        throw new UsageError(`unknown command "${command}"; fllama --help lists them.`);
    }
  } catch (error) {
    if (error instanceof UsageError) {
      io.err(`fllama: ${error.message}`);
      return 2;
    }
    io.err(`fllama: ${(error as Error).message}`);
    return 1;
  }
}

function passthrough(argv: string[], io: Io): Promise<number> {
  return new Promise((resolve) => {
    const child = spawn(process.env.FREELLAMA_OLLAMA_CLI ?? "ollama", argv, { stdio: "inherit" });
    child.on("exit", (code, signal) => resolve(signal ? 130 : code ?? 0));
    child.on("error", (error) => {
      io.err(`fllama: could not start the ollama CLI: ${error.message}. Install Ollama, or set FREELLAMA_OLLAMA_CLI.`);
      resolve(1);
    });
  });
}
