#!/usr/bin/env node
// Can a local model use the Octocode CLI smartly from the concise instructions alone? Runs graded
// questions through delegate_research (adapter octocode for local code, github for GitHub) one at
// a time and records the answer, the tool trail and whether it narrowed before reading.
// Usage: node scripts/local-test/octocode-agent.mjs <model> [serve] [local|github|all] [adapter-for-local]
import { spawn } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";

const MODEL = process.argv[2];
const SERVE = process.argv[3] ?? "http://127.0.0.1:11455";
const WHICH = process.argv[4] ?? "all";
const LOCAL_ADAPTER = process.argv[5] ?? "octocode";
if (!MODEL) { console.error("usage: octocode-agent.mjs <model> [serve] [local|github|all] [adapter]"); process.exit(2); }
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");

// Each `expect` lists facts that must ALL appear: one matching word is not a right answer.
const LOCAL = [
  { id: "L1", q: "Which source file samples the programs holding the most memory? Name the file.", expect: [/holders\.rs/] },
  { id: "L2", q: "Which function builds the request body sent to /v1/systemone? Give its name and file.", expect: [/build_decision_request/, /request\.rs/] },
  { id: "L3", q: "What is the value of the constant EMBEDDINGS_PER_JOB?", expect: [/\b32\b/] },
  { id: "L4", q: "Which files call the function memory_holders? List them.", expect: [/pipeline\.rs/, /select\.rs/] },
  { id: "L5", q: "What are the variants of the ModelType enum, and which file defines it?", expect: [/capability\.rs/, /Generative/, /Multimodal/, /EmbeddingOnly/, /Decision/, /Speech/] },
  { id: "L6", q: "What does the function requested_context in routing/profile.rs return for a decision task?", expect: [/8[_,]?192/] },
  { id: "L7", q: "Which MCP tools does packages/mcp/src/index.ts register? List their register functions.", expect: [/registerAudioTool/, /registerRunTaskTool/, /registerDoctorTool/] },
  { id: "L8", q: "Is there a function named frobnicate_widgets anywhere in this repository? Answer yes or no.", expect: [/\bno\b|not (found|exist|present)|does not|doesn't/i] },
];
const GITHUB = [
  { id: "G1", q: "In the GitHub repository ollama/ollama, which file defines the type SystemOneRequest?", expect: [/api\/types\.go/] },
  { id: "G2", q: "In ollama/ollama, what JSON fields does SystemOneRequest have? List them.", expect: [/keep_alive/, /questions/i, /state/i, /model/i] },
  { id: "G3", q: "Which pull request number added the System One scoring API to ollama/ollama?", expect: [/18606/] },
  { id: "G4", q: "Which GitHub repository hosts the npm package zod? Answer owner/repo.", expect: [/colinhacks\/zod/i] },
  { id: "G5", q: "According to the docs in ollama/ollama, what is the default response_format of /v1/audio/speech?", expect: [/\bwav\b/i] },
  { id: "G6", q: "What is the main programming language of the GitHub repository ollama/ollama?", expect: [/\bgo\b/i] },
];

const mcp = spawn("node", [path.join(ROOT, "packages/mcp/dist/index.js")], {
  cwd: ROOT, stdio: ["pipe", "pipe", "ignore"],
  env: { ...process.env, FREELLAMA_SERVE_ENDPOINT: SERVE, FREELLAMA_MCP_AUTOSTART_SERVE: "0", FREELLAMA_MCP_ALLOWED_ROOTS: ROOT },
});
let buffer = "";
const pending = new Map();
mcp.stdout.on("data", (chunk) => {
  buffer += chunk;
  let at;
  while ((at = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, at);
    buffer = buffer.slice(at + 1);
    try { const message = JSON.parse(line); pending.get(message.id)?.(message); } catch { /* not JSON */ }
  }
});
let id = 0;
const rpc = (method, params) => new Promise((resolve) => { const key = ++id; pending.set(key, resolve); mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: key, method, params })}\n`); });
await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "octocode-agent-check", version: "1" } });
mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);

const reportPath = process.env.FREELLAMA_AGENT_REPORT ?? path.join(ROOT, "LOCAL_OCTOCODE.md");
const lines = existsSync(reportPath) ? [readFileSync(reportPath, "utf8").trimEnd(), ""] : ["# LOCAL_OCTOCODE: can a local model use Octocode from the concise instructions?", "",
  "Instructions: docs/octocode/LOCAL.md and docs/octocode/GITHUB.md. Each question ran through delegate_research with the local model as the agent. One run per question on one machine, not a benchmark.", ""];
const out = (text = "") => { lines.push(text); console.error(text); };
const reads = new Set(["localFetch", "ghGetFileContent", "ghGetHistoryItem"]);

/** Wait until FreeLlama would admit this model, by a route preview that loads nothing. */
async function roomForModel(limitMs = Number(process.env.FREELLAMA_ROOM_WAIT_MS ?? 20 * 60_000)) {
  const until = Date.now() + limitMs;
  while (Date.now() < until) {
    try {
      const reply = await fetch(`${SERVE}/_freellama/v1/routes`, { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ task: "coding", model: MODEL, context_tokens: 8192 }) });
      const route = await reply.json();
      const status = String(route.execution?.resource_assessment?.status ?? route.error ?? `http ${reply.status}`);
      if (status.startsWith("ready")) return true;
      const holders = (route.execution?.memory_holders ?? []).slice(0, 4).map((holder) => holder.name).join(", ");
      console.error(`[${new Date().toISOString().slice(11, 19)}] ${MODEL}: ${status}${holders ? `; largest: ${holders}` : ""}; waiting`);
    } catch { /* serve restarting */ }
    await new Promise((resolve) => setTimeout(resolve, 15_000));
  }
  return false;
}

async function run(label, adapter, questions) {
  out(`## ${label}: \`${MODEL}\`, adapter \`${adapter}\`, ${new Date().toISOString()}\n`);
  out("| # | Question | Right | Calls | Tool trail | Verdict | s | Answer |\n|---|---|---|---:|---|---|---:|---|");
  let right = 0, calls = 0, narrowed = 0, total = 0;
  for (const c of questions) {
    if (!await roomForModel()) {
      out(`\nStopped at ${c.id}: FreeLlama did not admit \`${MODEL}\` within the wait, because the machine did not have room.\n`);
      break;
    }
    const t0 = performance.now();
    const reply = await rpc("tools/call", { name: "delegate_research", arguments: {
      question: c.q, workspacePath: ROOT, adapter, model: MODEL, endpoint: SERVE, receipt: "full", agent: { maxTurns: 8 },
    } });
    const result = reply.result ?? { isError: true, content: [{ text: JSON.stringify(reply.error) }] };
    const sc = result.structuredContent ?? {};
    const text = result.content?.map((part) => part.text ?? "").join(" ") ?? "";
    const answer = String(sc.answer ?? text);
    const trail = (Array.isArray(sc.evidence) ? sc.evidence : []).map((step) => String(step.tool ?? "?").replace("mcp.octocode.", ""));
    // A right answer with no tool call is a guess, not research.
    const ok = !result.isError && trail.length > 0 && c.expect.every((fact) => fact.test(answer));
    const firstRead = trail.findIndex((tool) => reads.has(tool));
    if (trail.length > 0) { total++; if (firstRead !== 0 || trail.length === 1) narrowed++; }
    right += ok; calls += trail.length;
    const seconds = ((performance.now() - t0) / 1000).toFixed(0);
    out(`| ${c.id} | ${c.q.slice(0, 70).replace(/\|/g, "/")} | ${ok ? "✅" : "❌"} | ${trail.length} | ${trail.join(" → ").slice(0, 90)} | ${sc.verification?.recommendation ?? (result.isError ? "error" : "?")} | ${seconds} | ${answer.replace(/\s+/g, " ").replace(/\|/g, "/").slice(0, 140)} |`);
    writeFileSync(reportPath, `${lines.join("\n")}\n`);
  }
  out(`\n**${right}/${questions.length}** right; ${(calls / questions.length).toFixed(1)} calls per question; narrowed before reading on ${narrowed}/${total}.\n`);
  writeFileSync(reportPath, `${lines.join("\n")}\n`);
}

if (WHICH !== "github") await run("Local tools", LOCAL_ADAPTER, LOCAL);
if (WHICH !== "local") await run("GitHub tools", "github", GITHUB);
await rpc("tools/call", { name: "ollama_manage", arguments: { action: "unload", model: MODEL, endpoint: SERVE } });
mcp.kill();
process.exit(0);
