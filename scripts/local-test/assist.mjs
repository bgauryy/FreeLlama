#!/usr/bin/env node
// Can FreeLlama's local models take routine work off a coding agent on a real repository?
// 1) issue -> code: embed per-file "cards" of the octocode native crates and rank them for each
//    real GitHub issue; 2) duplicates among real issues; 3) issue triage with the decision model;
// 4) OCR of real screens. Writes .octocode/local-test/LOCAL_ASSIST.md (git-ignored). Usage: node scripts/local-test/assist.mjs [serve]
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync, statSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SERVE = (process.argv[2] ?? "http://127.0.0.1:11435").replace(/\/$/, "");
const API = `${SERVE}/_freellama/v1`;
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const OCTO = path.resolve(ROOT, "../octocode/packages/octocode-native/crates");
const ISSUES = JSON.parse(readFileSync(path.join(ROOT, ".octocode/tmp/octocode-issues.json"), "utf8"));
const EMB = "nomic-embed-text:v1.5", DEC = "strands-decider:2b-nvfp4", OCR = "glm-ocr:latest";
const lines = [];
const log = (text = "") => { lines.push(text); console.error(text); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function http(method, url, body) {
  const t0 = performance.now();
  for (let attempt = 0; ; attempt++) {
    try {
      const r = await fetch(url.startsWith("http") ? url : `${API}${url}`, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
      const json = await r.json().catch(() => ({}));
      // A held host: wait for room rather than count the step as failed.
      if ([429, 503].includes(r.status) && attempt < 90) { await sleep(10_000); continue; }
      return { status: r.status, json, ms: Math.round(performance.now() - t0) };
    } catch (error) { return { status: 0, json: { error: String(error) }, ms: Math.round(performance.now() - t0) }; }
  }
}
async function embed(texts) {
  const vectors = [];
  for (let i = 0; i < texts.length; i += 32) {
    const r = await http("POST", "/tasks", { task: "embedding", model: EMB, input: texts.slice(i, i + 32), max_wait_seconds: 120 });
    if (r.status !== 200) throw new Error(`embedding failed: ${JSON.stringify(r.json).slice(0, 200)}`);
    vectors.push(...r.json.response.embeddings);
  }
  return vectors;
}
const cosine = (a, b) => { let d = 0, x = 0, y = 0; for (let i = 0; i < a.length; i++) { d += a[i] * b[i]; x += a[i] ** 2; y += b[i] ** 2; } return d / Math.sqrt(x * y); };
const tokens = (bytes) => Math.round(bytes / 4);

// ---------- 1. issue -> code ----------
function walk(dir) { return readdirSync(dir).flatMap((name) => { const p = path.join(dir, name); return statSync(p).isDirectory() ? walk(p) : p.endsWith(".rs") && !/tests?(\/|\.rs$|_tests\.rs$)/.test(p) ? [p] : []; }); }
function card(file) {
  const text = readFileSync(file, "utf8");
  const docs = text.split("\n").filter((l) => /^\s*\/\/[!/]/.test(l)).slice(0, 12).map((l) => l.replace(/^\s*\/\/[!/]\s?/, "")).join(" ");
  const items = [...text.matchAll(/\b(?:fn|struct|enum|trait|const|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)/g)].map((m) => m[1]).slice(0, 60).join(" ");
  return `search_document: ${path.relative(OCTO, file)} ${docs} ${items}`.slice(0, 3500);
}
const GROUND = [
  { issue: 477, expect: ["runtime/src/policy"] },
  { issue: 450, expect: ["runtime/src/policy/path"] },
  { issue: 479, expect: ["runtime/src/tools/lsp_search"] },
  { issue: 480, expect: ["runtime/src/tools/lsp_search"] },
  { issue: 478, expect: ["engine/src/signatures"] },
  { issue: 473, expect: ["runtime/src/tools/gh_clone_repo"] },
  { issue: 304, expect: ["runtime/src/tools/local_search"] },
];
log("## 1. From a real issue to the code it concerns (embeddings)\n");
const files = [...walk(path.join(OCTO, "runtime/src")), ...walk(path.join(OCTO, "engine/src"))];
const sourceBytes = files.reduce((sum, f) => sum + statSync(f).size, 0);
let t0 = performance.now();
const vectors = await embed(files.map(card));
const indexMs = Math.round(performance.now() - t0);
log(`Indexed ${files.length} source files (${(sourceBytes / 1e6).toFixed(1)} MB, about ${tokens(sourceBytes).toLocaleString()} tokens) as one card each in ${(indexMs / 1000).toFixed(1)} s.\n`);
log("| Issue | Top 5 files | Right area in top 5 | Query ms |\n|---|---|---|---:|");
let hits = 0;
for (const g of GROUND) {
  const issue = ISSUES.find((i) => i.number === g.issue);
  const q = `search_query: ${issue.title}\n${(issue.body ?? "").slice(0, 1500)}`;
  t0 = performance.now();
  const [v] = await embed([q]);
  const ranked = files.map((f, i) => [path.relative(OCTO, f), cosine(v, vectors[i])]).sort((a, b) => b[1] - a[1]).slice(0, 5);
  const ok = ranked.some(([f]) => g.expect.some((e) => f.startsWith(e)));
  hits += ok;
  log(`| #${g.issue} ${issue.title.slice(0, 60).replace(/\|/g, "/")} | ${ranked.map(([f]) => `\`${f}\``).join("<br>")} | ${ok ? "✅" : "❌"} | ${Math.round(performance.now() - t0)} |`);
}
log(`\n**${hits}/${GROUND.length}** issues put the right area in the top 5. An agent reads 5 short file cards (~${tokens(3500 * 5).toLocaleString()} tokens) instead of searching ${files.length} files.\n`);

// ---------- 2. duplicates ----------
log("## 2. Related or duplicate issues (embeddings)\n");
const PAIRS = [[140, 158], [257, 323], [289, 452], [411, 412], [414, 415], [357, 429]];
const issueText = (i) => `search_document: ${i.title}\n${(i.body ?? "").slice(0, 1200)}`;
const issueVectors = await embed(ISSUES.map(issueText));
log("| Issue | Nearest other issue | Expected | Match |\n|---|---|---|---|");
let pairHits = 0, pairTotal = 0;
for (const [a, b] of PAIRS) for (const [x, y] of [[a, b], [b, a]]) {
  const i = ISSUES.findIndex((issue) => issue.number === x);
  if (i < 0 || !ISSUES.some((issue) => issue.number === y)) continue;
  const nearest = ISSUES.map((issue, j) => [issue.number, j === i ? -1 : cosine(issueVectors[i], issueVectors[j])]).sort((p, q) => q[1] - p[1])[0];
  pairTotal++; pairHits += nearest[0] === y;
  log(`| #${x} | #${nearest[0]} (${nearest[1].toFixed(3)}) | #${y} | ${nearest[0] === y ? "✅" : "❌"} |`);
}
log(`\n**${pairHits}/${pairTotal}** found their related issue as the nearest neighbour.\n`);

// ---------- 3. triage ----------
log("## 3. Triage of 40 real issues (decision model)\n");
const LABEL = { bug: [480, 479, 478, 477, 452, 450, 373, 357, 323, 322, 304, 289, 272, 257, 194, 158, 132, 58, 43, 429], feature: [473, 443, 346, 325, 316, 256, 160, 143, 140, 126], security: [415, 414, 412, 411, 321], question: [413] };
const truth = Object.fromEntries(Object.entries(LABEL).flatMap(([label, ns]) => ns.map((n) => [n, label])));
const criteria = { bug: "Something is broken or behaves incorrectly", feature: "A request for new capability or configuration", security: "A security, privacy or supply-chain risk", question: "A question or other non-actionable post" };
let triageHits = 0, triageTotal = 0; const misses = [];
t0 = performance.now();
for (const issue of ISSUES) {
  if (!truth[issue.number]) continue;
  const r = await http("POST", `${SERVE}/v1/systemone`, { model: DEC, state: `${issue.title}\n${(issue.body ?? "").slice(0, 2000)}`, questions: { kind: { type: "choice", instructions: "What kind of GitHub issue is this?", criteria } } });
  const got = r.json?.answers?.kind?.choice;
  triageTotal++; if (got === truth[issue.number]) triageHits++; else misses.push(`#${issue.number} ${truth[issue.number]}→${got}`);
}
log(`**${triageHits}/${triageTotal}** matched my labels in ${((performance.now() - t0) / 1000).toFixed(1)} s total. Misses: ${misses.join(", ") || "none"}.\n`);

// ---------- 4. OCR of real screens ----------
log("## 4. Reading real screens (OCR)\n");
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const dir = path.join(tmpdir(), "freellama-assist"); mkdirSync(dir, { recursive: true });
const shoot = (url, file, size) => execFileSync(CHROME, ["--headless=new", "--disable-gpu", "--hide-scrollbars", "--virtual-time-budget=4000", `--screenshot=${file}`, `--window-size=${size}`, url], { stdio: "ignore" });
const ocr = async (file) => http("POST", "/tasks", { task: "vision", model: OCR, prompt: "Text Recognition:", images: [readFileSync(file).toString("base64")], stop_on_repetition: true, max_wait_seconds: 120, request_options: { options: { num_predict: 700 } } });
const log2 = readFileSync(path.join(ROOT, ".octocode/tmp/g-js.txt"), "utf8").split("\n").filter((l) => l.trim()).slice(-12).join("\n");
writeFileSync(path.join(dir, "log.html"), `<html><body style="margin:12px;background:#111"><pre style="font:20px Menlo;color:#ddd;margin:0">${log2.replace(/</g, "&lt;")}</pre></body></html>`);
const screens = [
  { label: "FreeLlama status page", url: `${SERVE}/_freellama/ui`, size: "1200,900", expect: ["FreeLlama"] },
  { label: "Terminal: test run summary", url: `file://${path.join(dir, "log.html")}`, size: "1100,420", expect: (log2.match(/Tests\s+\d+ passed/) ?? ["passed"]).concat((log2.match(/Test Files\s+\d+ passed/) ?? [])) },
];
log("| Screen | Expected text | Read back | ms |\n|---|---|---|---:|");
for (const s of screens) {
  const file = path.join(dir, `${s.label.replace(/\W+/g, "-")}.png`);
  shoot(s.url, file, s.size);
  const r = await ocr(file);
  const text = r.json?.response?.message?.content ?? r.json?.error ?? "";
  const norm = (x) => x.toLowerCase().replace(/\s+/g, " ");
  const missing = s.expect.filter((e) => !norm(text).includes(norm(e)));
  log(`| ${s.label} | ${s.expect.join(", ")} | ${missing.length ? `❌ missing ${missing.join(", ")}` : "✅"} — ${text.replace(/\s+/g, " ").slice(0, 120).replace(/\|/g, "/")} | ${r.ms} |`);
}

for (const model of [EMB, DEC, OCR]) await http("POST", "/unload", { model });
mkdirSync(path.join(ROOT, ".octocode", "local-test"), { recursive: true });
writeFileSync(path.join(ROOT, ".octocode", "local-test", "LOCAL_ASSIST.md"), `# LOCAL_ASSIST: local models doing an agent's routine work on a real repository

Real data: the octocode repository (${files.length} Rust source files of its native crates) and its
${ISSUES.length} most recent GitHub issues. Apple M2 Pro, 32 GB, Ollama 0.40.13, FreeLlama 0.8.0, with
other work running; each step waited for FreeLlama to admit its model. Ground truth for issue
locations and labels was set by reading the code and issues, before running.

${lines.join("\n")}
`);
console.error("done");
