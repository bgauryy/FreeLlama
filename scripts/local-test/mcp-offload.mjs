#!/usr/bin/env node
// Live check of the offload surface through the real MCP server: a decision task with a file as
// its state, `nearest` over real source files, and OCR of a screenshot named by path. One small
// model at a time, unloaded after use. Usage: node scripts/local-test/mcp-offload.mjs [serve]
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SERVE = process.argv[2] ?? "http://127.0.0.1:11455";
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const mcp = spawn("node", [path.join(ROOT, "packages/mcp/dist/index.js")], {
  cwd: ROOT, stdio: ["pipe", "pipe", "ignore"], env: { ...process.env, FREELLAMA_AUTOSTART: "0" },
});
let buffer = "";
const pending = new Map();
mcp.stdout.on("data", (chunk) => {
  buffer += chunk;
  let at;
  while ((at = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, at);
    buffer = buffer.slice(at + 1);
    try {
      const message = JSON.parse(line);
      pending.get(message.id)?.(message);
    } catch { /* not JSON */ }
  }
});
let id = 0;
const rpc = (method, params) => new Promise((resolve) => {
  const key = ++id;
  pending.set(key, resolve);
  mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: key, method, params })}\n`);
});
const call = async (name, args) => {
  const t0 = performance.now();
  const reply = await rpc("tools/call", { name, arguments: { endpoint: SERVE, ...args } });
  const result = reply.result ?? { isError: true, content: [{ text: JSON.stringify(reply.error) }] };
  return { ms: Math.round(performance.now() - t0), result, text: result.content?.map((part) => part.text ?? "").join("\n") ?? "" };
};

await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "offload-check", version: "1" } });
mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);

const lines = ["# MCP offload check", "", `serve ${SERVE}; ${new Date().toISOString()}`, ""];
const record = (label, outcome, extra = "") => { lines.push(`## ${label}`, "", "```text", outcome.text.trim(), "```", `${outcome.result.isError ? "ERROR" : "ok"} · ${outcome.ms} ms${extra}`, ""); console.error(label, outcome.result.isError ? "ERROR" : "ok", outcome.ms); };

// 1. Decision with a file as the state.
const issue = path.join(tmpdir(), "freellama-offload-issue.md");
writeFileSync(issue, "Title: Crash when saving a file with a long path\n\nSaving a file whose path is longer than 260 characters panics with index out of bounds in path_policy.rs.");
record("decision: issue file as state", await call("run_task", {
  task: "decision", model: "strands-decider:2b-nvfp4", files: [issue],
  questions: {
    kind: { type: "choice", instructions: "What kind of GitHub issue is this?", criteria: { bug: "Something is broken", feature: "A request for new capability", question: "A question" } },
    crash: { type: "noul", instructions: "Does the report describe a crash or panic?" },
  },
}));
await call("ollama_manage", { action: "unload", model: "strands-decider:2b-nvfp4" });

// 2. nearest over real source files named by a glob: files are chunked by lines on the server.
const glob = "packages/rust-core/src/platform/**/*.rs";
for (const query of [
  "search_query: which programs hold the most memory when a model load has to wait",
  "search_query: how a decision task request is built for /v1/systemone",
]) {
  record(`embedding nearest: ${query.slice(14)}`, await call("run_task", {
    task: "embedding", model: "nomic-embed-text:v1.5", files: [glob],
    nearest: { query, top: 3, documentPrefix: "search_document: " },
  }), ` · glob ${glob}`);
}
record("embedding nearest: duplicate pairs", await call("run_task", {
  task: "embedding", model: "nomic-embed-text:v1.5",
  input: ["search_document: the app crashes on save", "search_document: add dark mode", "search_document: saving a file makes the app crash", "search_document: support a dark theme"],
  nearest: { top: 2 },
}));
await call("ollama_manage", { action: "unload", model: "nomic-embed-text:v1.5" });

// 3. OCR of a screenshot named by path (no base64 in the call).
const shot = path.join(tmpdir(), "freellama-assist", "Terminal-test-run-summary.png");
record("vision: screenshot by path", await call("run_task", {
  task: "vision", model: "glm-ocr:latest", files: [shot], prompt: "Text Recognition:", stopOnRepetition: true, options: { num_predict: 400 },
}));
await call("ollama_manage", { action: "unload", model: "glm-ocr:latest" });

// 4. Refusals the agent should see before any load.
record("refusal: file outside roots", await call("run_task", { task: "completion", prompt: "p", files: ["/etc/hosts"] }));

writeFileSync(path.join(ROOT, "LOCAL_OFFLOAD.md"), `${lines.join("\n")}\n`);
mcp.kill();
process.exit(0);
