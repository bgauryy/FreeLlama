#!/usr/bin/env node
// Audio and multimodal embeddings through the real MCP server, one small model at a time:
// speak known sentences (text to speech), transcribe them back (speech to text) and score the word
// error; translate one; rank rendered screenshots against text queries with a multimodal
// embedding model; compare it with nomic on code search. Writes LOCAL_MEDIA.md.
// Usage: node scripts/local-test/media.mjs [serve]
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SERVE = process.argv[2] ?? "http://127.0.0.1:11455";
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const TTS = "phonon:en", ASR = "lfm3-audio:2b-q4_K_M", MM = "embeddinggemma-2:740m-q8_0", NOMIC = "nomic-embed-text:v1.5";
const dir = path.join(tmpdir(), "freellama-media");
mkdirSync(dir, { recursive: true });

const mcp = spawn("node", [path.join(ROOT, "packages/mcp/dist/index.js")], { cwd: ROOT, stdio: ["pipe", "pipe", "ignore"], env: { ...process.env, FREELLAMA_AUTOSTART: "0" } });
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
const call = async (name, args) => {
  const t0 = performance.now();
  const reply = await rpc("tools/call", { name, arguments: { endpoint: SERVE, ...args } });
  const result = reply.result ?? { isError: true, content: [{ text: JSON.stringify(reply.error) }] };
  return { ms: Math.round(performance.now() - t0), result, text: result.content?.map((part) => part.text ?? "").join("\n") ?? "" };
};
await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "media-check", version: "1" } });
mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);

const lines = ["# LOCAL_MEDIA: audio and multimodal embeddings through MCP", "", `serve ${SERVE}; ${new Date().toISOString()}; Apple M2 Pro 32 GB; Ollama 0.40.13`, ""];
const log = (text = "") => { lines.push(text); console.error(text); };
const words = (text) => text.toLowerCase().replace(/[^a-z0-9' ]+/g, " ").split(/\s+/).filter(Boolean);
/** Word error rate: word-level edit distance over the reference length. */
function wer(reference, hypothesis) {
  const r = words(reference), h = words(hypothesis);
  const d = Array.from({ length: r.length + 1 }, (_, i) => [i, ...Array(h.length).fill(0)]);
  for (let j = 1; j <= h.length; j++) d[0][j] = j;
  for (let i = 1; i <= r.length; i++) for (let j = 1; j <= h.length; j++) {
    d[i][j] = Math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (r[i - 1] === h[j - 1] ? 0 : 1));
  }
  return d[r.length][h.length] / Math.max(1, r.length);
}

// ---------- 1. speak, then transcribe back ----------
log("## 1. Text to speech, then speech to text (round trip)\n");
const SENTENCES = [
  "The build failed because the test runner could not find the configuration file.",
  "Please restart the server after you update the environment variables.",
  "Forty two tests passed and three were skipped.",
  "The memory limit was reached while loading the model.",
  "Open the pull request and assign it to the review team.",
];
log(`| # | Speak ${TTS} | Audio | Transcribe ${ASR} | Word error |\n|---|---:|---:|---|---:|`);
let totalWer = 0, spokenSeconds = 0, transcribeMs = 0;
for (const [index, sentence] of SENTENCES.entries()) {
  const file = path.join(dir, `s${index + 1}.wav`);
  const spoken = await call("audio", { action: "speak", model: TTS, text: sentence, outputFile: file });
  if (spoken.result.isError) { log(`| ${index + 1} | ❌ ${spoken.text.slice(0, 120)} | | | |`); continue; }
  const seconds = spoken.result.structuredContent?.seconds ?? 0;
  spokenSeconds += seconds;
  const heard = await call("audio", { action: "transcribe", model: ASR, file, language: "en" });
  const text = heard.result.isError ? `❌ ${heard.text.slice(0, 160)}` : String(heard.result.structuredContent?.text ?? "");
  const error = heard.result.isError ? 1 : wer(sentence, text);
  totalWer += error;
  transcribeMs += heard.ms;
  log(`| ${index + 1} | ${spoken.ms} ms | ${seconds} s | ${text.replace(/\|/g, "/").slice(0, 110)} (${heard.ms} ms) | ${(error * 100).toFixed(0)}% |`);
}
log(`\nMean word error **${((totalWer / SENTENCES.length) * 100).toFixed(1)}%** over ${SENTENCES.length} sentences; ${spokenSeconds.toFixed(1)} s of speech transcribed in ${(transcribeMs / 1000).toFixed(1)} s.\n`);
const translated = await call("audio", { action: "translate", model: ASR, file: path.join(dir, "s2.wav"), toLanguage: "fr" });
log(`Translate sentence 2 to French: ${translated.result.isError ? "❌ " : ""}${translated.text.replace(/\s+/g, " ").slice(0, 200)} (${translated.ms} ms)\n`);
await call("ollama_manage", { action: "unload", model: TTS });
await call("ollama_manage", { action: "unload", model: ASR });

// ---------- 2. multimodal embeddings: screenshots by text ----------
log("## 2. Find a screenshot by describing it (multimodal embeddings)\n");
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PAGES = {
  "login.png": `<h2>Sign in</h2><label>Email</label><input value="dev@example.com"><br><label>Password</label><input type="password" value="xxxxxxxx"><br><button style="background:#2563eb;color:#fff;padding:8px 24px">Log in</button>`,
  "chart.png": `<h2>Monthly revenue</h2><div style="display:flex;align-items:flex-end;gap:12px;height:220px">${[60, 90, 140, 120, 200, 170].map((h) => `<div style="width:40px;height:${h}px;background:#16a34a"></div>`).join("")}</div>`,
  "error.png": `<pre style="background:#111;color:#f87171;padding:12px;font:16px Menlo">error[E0308]: mismatched types\n  --> src/main.rs:12:5\n   |\n12 |     return "ok";\n   |            ^^^^ expected i32, found &str</pre>`,
  "table.png": `<h2>Team members</h2><table border="1" cellpadding="6"><tr><th>Name</th><th>Role</th><th>City</th></tr><tr><td>Ana</td><td>Engineer</td><td>Lisbon</td></tr><tr><td>Ben</td><td>Designer</td><td>Oslo</td></tr><tr><td>Chen</td><td>Manager</td><td>Taipei</td></tr></table>`,
  "map.png": `<h2>Route</h2><svg width="360" height="220"><rect width="360" height="220" fill="#e0f2fe"/><path d="M20 200 C 120 20, 240 220, 340 30" stroke="#ea580c" stroke-width="6" fill="none"/><circle cx="20" cy="200" r="10" fill="#dc2626"/><circle cx="340" cy="30" r="10" fill="#16a34a"/></svg>`,
};
for (const [name, html] of Object.entries(PAGES)) {
  const page = path.join(dir, `${name}.html`);
  writeFileSync(page, `<html><body style="font-family:sans-serif;margin:16px;background:#fff">${html}</body></html>`);
  execFileSync(CHROME, ["--headless=new", "--disable-gpu", "--hide-scrollbars", `--screenshot=${path.join(dir, name)}`, "--window-size=480,320", `file://${page}`], { stdio: "ignore" });
}
const shots = Object.keys(PAGES).map((name) => path.join(dir, name));
const QUERIES = {
  "login.png": "a sign-in form with email and password fields",
  "chart.png": "a bar chart of revenue over months",
  "error.png": "a compiler error message in a terminal",
  "table.png": "a table listing people, their roles and cities",
  "map.png": "a map with a route between two points",
};
log("| Query | Top match | Right |\n|---|---|---|");
let imageHits = 0, imageMs = 0;
for (const [expected, query] of Object.entries(QUERIES)) {
  const ranked = await call("run_task", { task: "embedding", model: MM, files: shots, nearest: { query: `task: search result | query: ${query}`, top: 1 } });
  const top = ranked.result.structuredContent?.nearest?.matches?.[0]?.label ?? `❌ ${ranked.text.slice(0, 160)}`;
  const ok = String(top).endsWith(expected);
  imageHits += ok;
  imageMs += ranked.ms;
  log(`| ${query} | ${path.basename(String(top))} | ${ok ? "✅" : "❌"} |`);
}
log(`\n**${imageHits}/${Object.keys(QUERIES).length}** screenshots found by description (${Math.round(imageMs / Object.keys(QUERIES).length)} ms per query, ${shots.length} images embedded each time).\n`);

// ---------- 3. text: embeddinggemma-2 vs nomic on code search ----------
log("## 3. Code search: embeddinggemma-2 against nomic\n");
const CODE = [
  { query: "how a decision task request is built for /v1/systemone", expect: "execution/request.rs" },
  { query: "which programs hold the most memory when a model load has to wait", expect: "resources/holders.rs" },
  { query: "unload idle models when the host is under memory pressure", expect: "sweep.rs" },
];
log("| Query | nomic top 3 | embeddinggemma-2 top 3 |\n|---|---|---|");
const prefixes = { [NOMIC]: ["search_query: ", "search_document: "], [MM]: ["task: search result | query: ", "title: none | text: "] };
const hits = { [NOMIC]: 0, [MM]: 0 };
const rows = [];
for (const item of CODE) {
  const cells = [];
  for (const model of [NOMIC, MM]) {
    const [queryPrefix, documentPrefix] = prefixes[model];
    const ranked = await call("run_task", { task: "embedding", model, files: ["packages/rust-core/src/platform/**/*.rs"], nearest: { query: queryPrefix + item.query, top: 3, documentPrefix } });
    const labels = (ranked.result.structuredContent?.nearest?.matches ?? []).map((match) => match.label.replace("packages/rust-core/src/platform/", ""));
    const ok = labels.some((label) => label.startsWith(item.expect));
    hits[model] += ok;
    cells.push(`${ok ? "✅" : "❌"} ${labels.join("<br>") || ranked.text.slice(0, 120)} (${(ranked.ms / 1000).toFixed(1)} s)`);
  }
  rows.push(`| ${item.query} | ${cells.join(" | ")} |`);
}
for (const row of rows) log(row);
log(`\nRight file in the top 3: nomic **${hits[NOMIC]}/${CODE.length}**, embeddinggemma-2 **${hits[MM]}/${CODE.length}**.\n`);
for (const model of [NOMIC, MM]) await call("ollama_manage", { action: "unload", model });

writeFileSync(path.join(ROOT, "LOCAL_MEDIA.md"), `${lines.join("\n")}\n`);
mcp.kill();
process.exit(0);
