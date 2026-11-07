#!/usr/bin/env node
// LOCAL_TEST: 100 graded questions for what FreeLlama's local models can take off an agent's plate
// (OCR, classification, small coding). Runs one question at a time against a FreeLlama serve,
// waits for FreeLlama to admit each model group instead of forcing a load, and rewrites
// LOCAL_TEST.md after every answer. Usage: node scripts/local-test/run.mjs [serveEndpoint]
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { CASES } from "./cases.mjs";

const SERVE = (process.argv[2] ?? process.env.FREELLAMA_SERVE_ENDPOINT ?? "http://127.0.0.1:11435").replace(/\/$/, "");
const API = `${SERVE}/_freellama/v1`;
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const RESULTS = path.join(ROOT, ".octocode/tmp/local-test-results.json");
const REPORT = path.join(ROOT, "LOCAL_TEST.md");
const IMAGES = path.join(tmpdir(), "freellama-local-test-images");
/** How long one model group may wait for FreeLlama to admit it before it is reported as held. */
const GROUP_WAIT_MS = Number(process.env.LOCAL_TEST_GROUP_WAIT_MS ?? 30 * 60_000);
/** How often a held model is rechecked; each check is a route preview that loads nothing. */
const RECHECK_MS = Number(process.env.LOCAL_TEST_RECHECK_MS ?? 10_000);

const results = existsSync(RESULTS) ? JSON.parse(readFileSync(RESULTS, "utf8")) : {};
const save = () => { mkdirSync(path.dirname(RESULTS), { recursive: true }); writeFileSync(RESULTS, JSON.stringify(results, null, 2)); writeFileSync(REPORT, report()); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const norm = (s) => String(s ?? "").toLowerCase().replace(/\s+/g, " ").trim();

async function http(method, url, body) {
  const t0 = performance.now();
  let r;
  try {
    r = await fetch(url, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  } catch (error) {
    // The server restarting or not yet listening is "not ready", not a failed question.
    return { status: 0, json: { error: `unreachable: ${error.cause?.code ?? error.message}` }, ms: Math.round(performance.now() - t0) };
  }
  const text = await r.text();
  let json; try { json = JSON.parse(text); } catch { json = { raw: text }; }
  return { status: r.status, json, ms: Math.round(performance.now() - t0) };
}

// --- images for OCR: the text as a tight HTML block, screenshotted by headless Chrome ---
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
function image(id, lines) {
  mkdirSync(IMAGES, { recursive: true });
  const base = path.join(IMAGES, id);
  const escaped = lines.join("\n").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  writeFileSync(`${base}.html`, `<html><body style="margin:16px;background:#fff"><pre style="font:26px Menlo,monospace;color:#000;margin:0;white-space:pre-wrap">${escaped}</pre></body></html>`);
  const height = 40 + lines.length * 36;
  execFileSync(CHROME, ["--headless=new", "--disable-gpu", "--hide-scrollbars", `--screenshot=${base}.png`, `--window-size=1100,${height}`, `file://${base}.html`], { stdio: "ignore" });
  return readFileSync(`${base}.png`).toString("base64");
}

// --- admission gate: ask FreeLlama (preview, loads nothing) whether a model fits now ---
async function waitForRoom(model, task) {
  const started = Date.now();
  for (;;) {
    const r = await http("POST", `${API}/routes`, { task, model, ...(task === "completion" || task === "coding" ? { context_tokens: 4096 } : {}) });
    const status = r.json?.execution?.resource_assessment?.status ?? r.json?.error ?? `http ${r.status}`;
    if (String(status).startsWith("ready")) return { ok: true, waited_ms: Date.now() - started };
    if (Date.now() - started > GROUP_WAIT_MS) return { ok: false, status, waited_ms: Date.now() - started };
    console.error(`[${new Date().toISOString().slice(11, 19)}] ${model}: ${status}; waiting for room`);
    await sleep(RECHECK_MS);
  }
}

const unload = (model) => http("POST", `${API}/unload`, { model });

// --- runners per kind; each returns {answer, ok, ms, detail} ---
async function runOcr(c) {
  const r = await http("POST", `${API}/tasks`, { task: "vision", model: "glm-ocr:latest", prompt: "Text Recognition:",
    images: [image(c.id, c.lines)], max_wait_seconds: 120, stop_on_repetition: true, request_options: { options: { num_predict: 400 } } });
  const answer = r.json?.response?.message?.content ?? r.json?.error ?? "";
  const missing = c.expect.filter((k) => !norm(answer).includes(norm(k)));
  return { answer, ok: r.status === 200 && missing.length === 0, ms: r.ms, detail: missing.length ? `missing: ${missing.join(" | ")}` : "" };
}

async function runDecide(c) {
  // Raw passthrough: a held host answers 429/503; wait for room rather than count it as wrong.
  let r;
  const started = Date.now();
  for (;;) {
    r = await http("POST", `${SERVE}/v1/systemone`, { model: "strands-decider:2b-nvfp4", state: c.state, questions: { q: c.question } });
    if (![429, 503].includes(r.status) || Date.now() - started > GROUP_WAIT_MS) break;
    console.error(`${c.id}: decider held (${r.status}); waiting for room`);
    await sleep(RECHECK_MS);
  }
  const a = r.json?.answers?.q;
  let answer;
  if (c.question.type === "choice") answer = a?.choice ?? a?.value ?? JSON.stringify(a);
  else if (c.question.type === "noul") { const p = typeof a === "number" ? a : (a?.noul ?? a?.probability ?? a?.value); answer = typeof p === "number" ? (p >= 0.5 ? "true" : "false") + ` (${p.toFixed(2)})` : JSON.stringify(a ?? r.json); }
  else { const s = typeof a === "number" ? a : (a?.score ?? a?.value); answer = typeof s === "number" ? String(Math.round(s)) + ` (${s.toFixed(2)})` : JSON.stringify(a ?? r.json); }
  const got = String(answer).split(" ")[0];
  return { answer, ok: r.status === 200 && got === String(c.expect), ms: r.ms, detail: r.status === 200 ? "" : JSON.stringify(r.json).slice(0, 200) };
}

let centroids;
async function embed(input) {
  const r = await http("POST", `${API}/tasks`, { task: "embedding", model: "nomic-embed-text:v1.5", input, max_wait_seconds: 120 });
  if (r.status !== 200) throw new Error(JSON.stringify(r.json).slice(0, 200));
  return { vectors: r.json.response.embeddings, ms: r.ms };
}
const cosine = (a, b) => { let d = 0, x = 0, y = 0; for (let i = 0; i < a.length; i++) { d += a[i] * b[i]; x += a[i] ** 2; y += b[i] ** 2; } return d / Math.sqrt(x * y); };
async function runEmbed(c, training) {
  if (!centroids) {
    centroids = {};
    for (const [label, examples] of Object.entries(training)) {
      const { vectors } = await embed(examples.map((e) => `classification: ${e}`));
      centroids[label] = vectors[0].map((_, i) => vectors.reduce((s, v) => s + v[i], 0) / vectors.length);
    }
  }
  const { vectors, ms } = await embed([`classification: ${c.text}`]);
  const ranked = Object.entries(centroids).map(([label, v]) => [label, cosine(vectors[0], v)]).sort((a, b) => b[1] - a[1]);
  return { answer: `${ranked[0][0]} (${ranked[0][1].toFixed(3)} vs ${ranked[1][0]} ${ranked[1][1].toFixed(3)})`, ok: ranked[0][0] === c.expect, ms, detail: "" };
}

// The coding model: FREELLAMA_CODER (default gpt-oss:20b); FREELLAMA_CODER_THINK=none for a model
// without the thinking capability.
const CODER = process.env.FREELLAMA_CODER ?? "gpt-oss:20b";
const CODER_THINK = process.env.FREELLAMA_CODER_THINK ?? "low";
async function runCode(c) {
  const r = await http("POST", `${API}/tasks`, { task: c.task ?? "coding", model: CODER, context_tokens: 4096, max_wait_seconds: 120,
    messages: [{ role: "system", content: "Answer with only what is asked: no explanation, no code fences unless asked." }, { role: "user", content: c.prompt }],
    request_options: { ...(CODER_THINK === "none" ? {} : { think: CODER_THINK }), options: { num_predict: 400 } } });
  const answer = r.json?.response?.message?.content ?? r.json?.error ?? "";
  const ok = r.status === 200 && (c.expect instanceof RegExp ? c.expect.test(answer) : norm(answer).includes(norm(c.expect)));
  return { answer, ok, ms: r.ms, detail: r.status === 200 ? "" : JSON.stringify(r.json).slice(0, 200) };
}

let mcp;
async function runAgent(c) {
  if (!mcp) {
    const require = createRequire(path.join(ROOT, "packages/mcp/package.json"));
    const { Client } = require("@modelcontextprotocol/sdk/client/index.js");
    const { StdioClientTransport } = require("@modelcontextprotocol/sdk/client/stdio.js");
    mcp = new Client({ name: "local-test", version: "0" });
    await mcp.connect(new StdioClientTransport({ command: "node", args: [path.join(ROOT, "packages/mcp/dist/index.js")],
      env: { ...process.env, FREELLAMA_SERVE_ENDPOINT: SERVE, FREELLAMA_MCP_AUTOSTART_SERVE: "0", FREELLAMA_MCP_ALLOWED_ROOTS: ROOT }, stderr: "pipe" }));
  }
  const t0 = performance.now();
  const r = await mcp.callTool({ name: "delegate_research", arguments: { question: c.prompt, workspacePath: path.join(ROOT, c.path ?? ""), model: CODER } }, undefined, { timeout: 600_000 });
  const sc = r.structuredContent ?? {};
  const answer = `${sc.answer ?? r.content?.map((x) => x.text).join(" ")} [verdict: ${sc.verification?.recommendation ?? "?"}]`;
  const ok = !r.isError && (c.expect instanceof RegExp ? c.expect.test(answer) : norm(answer).includes(norm(c.expect)));
  return { answer, ok, ms: Math.round(performance.now() - t0), detail: "" };
}

const GROUPS = [
  { key: "embed", title: "Classification with embeddings (nomic-embed-text:v1.5)", model: "nomic-embed-text:v1.5", task: "embedding", run: (c) => runEmbed(c, CASES.embedTraining) },
  { key: "decide", title: "Classification with the decision model (strands-decider:2b-nvfp4)", model: "strands-decider:2b-nvfp4", task: "completion", gate: false, run: runDecide },
  { key: "ocr", title: "OCR (glm-ocr:latest)", model: "glm-ocr:latest", task: "vision", run: runOcr },
  { key: "code", title: `Small coding tasks (${CODER})`, model: CODER, task: "coding", run: runCode },
  { key: "agent", title: `Small research agent: delegate_research (${CODER})`, model: CODER, task: "coding", run: runAgent },
];

function report() {
  const rows = (g) => CASES[g.key].map((c, i) => {
    const r = results[c.id];
    const q = (c.question?.instructions ? `${c.state} — ${c.question.instructions}` : c.prompt ?? c.text ?? c.lines.join(" / ")).replace(/\|/g, "\\|").replace(/\n/g, " ");
    const exp = String(c.expect instanceof RegExp ? c.expect.source : Array.isArray(c.expect) ? c.expect.join(", ") : c.expect).replace(/\|/g, "\\|");
    const ans = r ? String(r.answer ?? "").replace(/\s+/g, " ").replace(/\|/g, "\\|").slice(0, 140) : "";
    const mark = !r ? "…" : r.skipped ? "held" : r.ok ? "✅" : "❌";
    return `| ${c.id} | ${q.slice(0, 160)} | ${exp.slice(0, 80)} | ${ans} | ${mark} | ${r?.ms ?? ""} |`;
  }).join("\n");
  const tally = (g) => { const rs = CASES[g.key].map((c) => results[c.id]).filter(Boolean); const run = rs.filter((r) => !r.skipped); return `${run.filter((r) => r.ok).length}/${run.length} passed${rs.length - run.length ? `, ${rs.length - run.length} held` : ""}, ${CASES[g.key].length - rs.length} pending`; };
  return `# LOCAL_TEST: what FreeLlama's local models can take off an agent's plate

100 graded questions, run one at a time through FreeLlama (\`scripts/local-test/run.mjs\`), each with a
known answer. Before each model group the harness asks FreeLlama (a route preview, which loads nothing)
whether the model fits in memory now, and waits instead of forcing a load. Machine: Apple M2 Pro,
32 GB unified memory, Ollama 0.40.13, FreeLlama 0.8.0 serve at ${SERVE}. Results are one run on one
machine, not a benchmark.

| Group | Model | Result |
|---|---|---|
${GROUPS.map((g) => `| ${g.title.split(" (")[0]} | \`${g.model}\` | ${tally(g)} |`).join("\n")}

Grading: OCR passes when every expected phrase appears in the transcript (case and spacing ignored);
classification when the top label equals the expected one; coding and agent answers when the
expected text (or pattern) appears in the answer. "held" means FreeLlama did not admit the model
within ${Math.round(GROUP_WAIT_MS / 60000)} minutes because the machine did not have room for it.

${GROUPS.map((g) => `## ${g.title}\n\n| # | Question | Expected | Answer | Pass | ms |\n|---|---|---|---|---|---:|\n${rows(g)}\n`).join("\n")}
`;
}

for (const g of GROUPS) {
  const pending = CASES[g.key].filter((c) => !results[c.id] || results[c.id].skipped || results[c.id].error);
  if (pending.length === 0) continue;
  if (g.gate !== false) {
    const room = await waitForRoom(g.model, g.task);
    if (!room.ok) {
      for (const c of pending) results[c.id] = { skipped: true, answer: `held: ${room.status}`, ok: false };
      save(); continue;
    }
  }
  for (const c of pending) {
    try { results[c.id] = await g.run(c); }
    catch (e) { results[c.id] = { answer: String(e.message ?? e), ok: false, error: true }; }
    console.error(`${c.id} ${results[c.id].ok ? "PASS" : "FAIL"} ${results[c.id].ms ?? ""}ms`);
    save();
  }
  await unload(g.model);
}
if (mcp) await mcp.close();
save();
console.error("done");
