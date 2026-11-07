#!/usr/bin/env node
// Head-to-head: the same questions on rival models for each job, three rounds, one model at a time.
// A model runs when FreeLlama would admit it (a route preview loads nothing); smaller groups go first,
// larger ones wait for room. Progress is saved, so a rerun resumes. Writes LOCAL_RIVALS.md.
// Usage: node scripts/local-test/rivals.mjs [serve]
import { execFileSync, spawn } from "node:child_process";
import { existsSync, globSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { CASES } from "./cases.mjs";

const SERVE = (process.argv[2] ?? "http://127.0.0.1:11455").replace(/\/$/, "");
const API = `${SERVE}/_freellama/v1`;
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const STORE = path.join(ROOT, ".octocode/tmp/rivals.json");
const REPORT = path.join(ROOT, "LOCAL_RIVALS.md");
const WORK = path.join(tmpdir(), "freellama-rivals");
const ROUNDS = 3;
const NEMOTRON = "hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M";
const LIMIT_MS = Number(process.env.RIVALS_LIMIT_MS ?? 8 * 3600_000);
mkdirSync(WORK, { recursive: true });
mkdirSync(path.dirname(STORE), { recursive: true });
const results = existsSync(STORE) ? JSON.parse(readFileSync(STORE, "utf8")) : {};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const log = (text) => console.error(`[${new Date().toISOString().slice(11, 19)}] ${text}`);
const median = (values) => { const v = [...values].sort((a, b) => a - b); return v.length ? v[Math.floor(v.length / 2)] : 0; };

async function http(url, body) {
  const t0 = performance.now();
  try {
    const r = await fetch(url.startsWith("http") ? url : `${API}${url}`, body === undefined ? {} : { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
    const text = await r.text();
    let json; try { json = JSON.parse(text); } catch { json = { raw: text }; }
    return { status: r.status, json, ms: performance.now() - t0 };
  } catch (error) {
    return { status: 0, json: { error: String(error.cause?.code ?? error.message) }, ms: performance.now() - t0 };
  }
}
/** A hold is not a wrong answer: wait for room and ask again. */
async function asked(url, body) {
  for (let attempt = 0; ; attempt++) {
    const r = await http(url, body);
    const held = [0, 429, 503].includes(r.status) || r.json?.code === "resource_admission_unavailable";
    if (!held || attempt >= 120) return r;
    await sleep(15_000);
  }
}
async function hostLoad() {
  const status = (await http("/status")).json;
  return { available_gb: Number(((status.host?.available_memory_bytes ?? 0) / 1e9).toFixed(1)), load: status.host?.load_average_one_minute ?? null };
}
const admitted = async (task, model) => String((await http("/routes", { task, model })).json?.execution?.resource_assessment?.status ?? "").startsWith("ready");
const unload = (model) => http("/unload", { model });

// ---------- classify ----------
const ISSUE_LABELS = { bug: [480, 479, 478, 477, 452, 450, 373, 357, 323, 322, 304, 289, 272, 257, 194, 158, 132, 58, 43, 429], feature: [473, 443, 346, 325, 316, 256, 160, 143, 140, 126], security: [415, 414, 412, 411, 321], question: [413] };
const ISSUES = JSON.parse(readFileSync(path.join(ROOT, ".octocode/tmp/octocode-issues.json"), "utf8"));
const KIND = { type: "choice", instructions: "What kind of GitHub issue is this?", criteria: { bug: "Something is broken or behaves incorrectly", feature: "A request for new capability or configuration", security: "A security, privacy or supply-chain risk", question: "A question or other non-actionable post" } };
const CLASSIFY = [
  ...CASES.decide.map((c) => ({ id: c.id, state: c.state, question: c.question, expect: String(c.expect) })),
  ...Object.entries(ISSUE_LABELS).flatMap(([label, numbers]) => numbers.map((number) => {
    const issue = ISSUES.find((item) => item.number === number);
    return issue && { id: `#${number}`, state: `${issue.title}\n${(issue.body ?? "").slice(0, 2000)}`, question: KIND, expect: label };
  })).filter(Boolean),
];
function decided(answer) {
  if (answer?.type === "choice") return String(answer.choice);
  if (answer?.type === "noul") return answer.noul >= 0.5 ? "true" : "false";
  if (answer?.type === "score") return String(Math.round(answer.score));
  return "?";
}
async function classify(model) {
  const rows = {}; const times = [];
  for (let round = 0; round < ROUNDS; round++) for (const c of CLASSIFY) {
    const r = await asked("/tasks", { task: "decision", model, state: c.state, questions: { q: c.question }, max_wait_seconds: 120 });
    const got = r.status === 200 ? decided(r.json?.response?.answers?.q) : `error ${r.status}`;
    (rows[c.id] ??= []).push(got === c.expect);
    if (round > 0 || times.length > 0) times.push(r.ms);
    if (r.status !== 200) log(`${model} ${c.id}: ${got} ${JSON.stringify(r.json).slice(0, 160)}`);
  }
  return summarize(rows, times);
}
function summarize(rows, times) {
  const ids = Object.keys(rows);
  const perRound = Array.from({ length: ROUNDS }, (_, round) => ids.filter((id) => rows[id][round]).length);
  const flaky = ids.filter((id) => new Set(rows[id]).size > 1);
  const misses = ids.filter((id) => rows[id].filter(Boolean).length * 2 < rows[id].length);
  return { total: ids.length, perRound, flaky, misses, median_ms: Math.round(median(times)) };
}

// ---------- OCR ----------
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
function ocrImage(c) {
  const base = path.join(WORK, c.id);
  if (!existsSync(`${base}.png`)) {
    const escaped = c.lines.join("\n").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    writeFileSync(`${base}.html`, `<html><body style="margin:16px;background:#fff"><pre style="font:26px Menlo,monospace;color:#000;margin:0;white-space:pre-wrap">${escaped}</pre></body></html>`);
    execFileSync(CHROME, ["--headless=new", "--disable-gpu", "--hide-scrollbars", `--screenshot=${base}.png`, `--window-size=1100,${40 + c.lines.length * 36}`, `file://${base}.html`], { stdio: "ignore" });
  }
  return readFileSync(`${base}.png`).toString("base64");
}
const norm = (text) => String(text ?? "").toLowerCase().replace(/\s+/g, " ").trim();
async function ocr(model) {
  const rows = {}; const times = [];
  for (let round = 0; round < ROUNDS; round++) for (const c of CASES.ocr) {
    const r = await asked("/tasks", { task: "vision", model, prompt: "Text Recognition:", images: [ocrImage(c)], max_wait_seconds: 120, stop_on_repetition: true, request_options: { options: { num_predict: 400 } } });
    const text = r.json?.response?.message?.content ?? "";
    (rows[c.id] ??= []).push(r.status === 200 && c.expect.every((phrase) => norm(text).includes(norm(phrase))));
    if (round > 0) times.push(r.ms);
    if (r.status !== 200) log(`${model} ${c.id}: ${r.status} ${JSON.stringify(r.json).slice(0, 160)}`);
  }
  return summarize(rows, times);
}

// ---------- speech to text ----------
const SENTENCES = [
  "The build failed because the test runner could not find the configuration file.",
  "Please restart the server after you update the environment variables.",
  "Forty two tests passed and three were skipped.",
  "The memory limit was reached while loading the model.",
  "Open the pull request and assign it to the review team.",
  "Deploy the new version to staging before the end of the week.",
  "The database migration took twelve minutes and finished without errors.",
  "Users reported that the login page is slow on mobile phones.",
  "Add a retry with exponential backoff when the request times out.",
  "We removed the old cache because it returned stale results.",
  "The nightly job failed at three in the morning with a timeout.",
  "Check the logs in the monitoring dashboard before you roll back.",
];
const NUMBERS = { zero: 0, one: 1, two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8, nine: 9, ten: 10, eleven: 11, twelve: 12, thirteen: 13, fourteen: 14, fifteen: 15, sixteen: 16, seventeen: 17, eighteen: 18, nineteen: 19, twenty: 20, thirty: 30, forty: 40, fifty: 50, sixty: 60, seventy: 70, eighty: 80, ninety: 90 };
/** Words, with "forty two" and "42" read as the same number: a format, not a transcription error. */
function words(text) {
  const out = [];
  for (const word of text.toLowerCase().replace(/[^a-z0-9' ]+/g, " ").split(/\s+/).filter(Boolean)) {
    const value = NUMBERS[word];
    if (value === undefined) { out.push(word); continue; }
    const last = Number(out.at(-1));
    if (value < 10 && last >= 20 && last % 10 === 0) out[out.length - 1] = String(last + value);
    else out.push(String(value));
  }
  return out;
}
function wer(reference, hypothesis) {
  const r = words(reference), h = words(hypothesis);
  const d = Array.from({ length: r.length + 1 }, (_, i) => [i, ...Array(h.length).fill(0)]);
  for (let j = 1; j <= h.length; j++) d[0][j] = j;
  for (let i = 1; i <= r.length; i++) for (let j = 1; j <= h.length; j++) d[i][j] = Math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (r[i - 1] === h[j - 1] ? 0 : 1));
  return d[r.length][h.length] / Math.max(1, r.length);
}
async function speakAll() {
  for (const [index, sentence] of SENTENCES.entries()) {
    const file = path.join(WORK, `s${index + 1}.wav`);
    if (existsSync(file)) continue;
    for (;;) {
      const r = await fetch(`${SERVE}/v1/audio/speech`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ model: "phonon:en", input: sentence, response_format: "wav" }) });
      if (r.ok) { writeFileSync(file, Buffer.from(await r.arrayBuffer())); break; }
      log(`speak held: ${r.status}`);
      await sleep(15_000);
    }
  }
  await unload("phonon:en");
}
async function audioCall(route, model, index, extra = {}) {
  for (let attempt = 0; ; attempt++) {
    const form = new FormData();
    form.append("file", new Blob([readFileSync(path.join(WORK, `s${index + 1}.wav`))], { type: "audio/wav" }), `s${index + 1}.wav`);
    form.append("model", model);
    for (const [key, value] of Object.entries(extra)) form.append(key, value);
    const t0 = performance.now();
    const r = await fetch(`${SERVE}/v1/audio/${route}`, { method: "POST", body: form });
    const text = await r.text();
    if (r.status !== 503 && r.status !== 429 || attempt >= 120) { let json = {}; try { json = JSON.parse(text); } catch { /* not JSON */ } return { ok: r.ok, text: json.text ?? text.slice(0, 200), ms: performance.now() - t0 }; }
    await sleep(15_000);
  }
}
async function speech(model) {
  await speakAll();
  const rates = []; const perfect = Array(ROUNDS).fill(0); const times = []; const samples = [];
  for (let round = 0; round < ROUNDS; round++) for (const [index, sentence] of SENTENCES.entries()) {
    const r = await audioCall("transcriptions", model, index, { language: "en", response_format: "json" });
    const rate = r.ok ? wer(sentence, r.text) : 1;
    rates.push(rate); if (rate === 0) perfect[round]++;
    if (round > 0 || index > 0) times.push(r.ms);
    if (rate > 0 && round === 0) samples.push(r.text.slice(0, 90));
  }
  const fr = await audioCall("translations", model, 1, { to_language: "fr", response_format: "json" });
  const es = await audioCall("translations", model, 4, { to_language: "es", response_format: "json" });
  return { total: SENTENCES.length, perRound: perfect, wer: Number((rates.reduce((a, b) => a + b, 0) / rates.length * 100).toFixed(1)), median_ms: Math.round(median(times)), samples, translate: { fr: fr.text.slice(0, 120), es: es.text.slice(0, 120), ok_fr: fr.ok && /serveur|redémarr|environnement/i.test(fr.text), ok_es: es.ok && /solicitud|equipo|revisi/i.test(es.text) } };
}

// ---------- text to speech: speak known sentences, then transcribe them with one fixed judge ----------
const SPEECH_JUDGE = "lfm3-audio:2b-q4_K_M";
async function textToSpeech(model) {
  const times = []; const seconds = []; const rounds = [];
  for (let round = 0; round < ROUNDS; round++) {
    const files = [];
    for (const [index, sentence] of SENTENCES.entries()) {
      const file = `tts-${model.replace(/\W+/g, "-")}-${round}-${index}`;
      for (let attempt = 0; ; attempt++) {
        const t0 = performance.now();
        const r = await fetch(`${SERVE}/v1/audio/speech`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ model, input: sentence, response_format: "wav" }) });
        if (r.ok) {
          const audio = Buffer.from(await r.arrayBuffer());
          writeFileSync(path.join(WORK, `${file}.wav`), audio);
          if (round > 0 || index > 0) times.push(performance.now() - t0);
          seconds.push(Math.max(0, audio.length - 44) / 48_000);
          files.push({ file, sentence });
          break;
        }
        if (![429, 503].includes(r.status) || attempt >= 120) throw new Error(`speak: ${r.status} ${(await r.text()).slice(0, 200)}`);
        await sleep(15_000);
      }
    }
    await unload(model);
    let total = 0, perfect = 0;
    for (const { file, sentence } of files) {
      const heard = await transcribeFile(path.join(WORK, `${file}.wav`));
      const rate = heard.ok ? wer(sentence, heard.text) : 1;
      total += rate; if (rate === 0) perfect++;
    }
    await unload(SPEECH_JUDGE);
    rounds.push({ wer: Number((total / files.length * 100).toFixed(1)), perfect });
  }
  return { total: SENTENCES.length, rounds, median_ms: Math.round(median(times)), audio_seconds: Number((seconds.reduce((a, b) => a + b, 0) / seconds.length).toFixed(1)) };
}
async function transcribeFile(file) {
  for (let attempt = 0; ; attempt++) {
    const form = new FormData();
    form.append("file", new Blob([readFileSync(file)], { type: "audio/wav" }), path.basename(file));
    form.append("model", SPEECH_JUDGE);
    form.append("language", "en");
    form.append("response_format", "json");
    const r = await fetch(`${SERVE}/v1/audio/transcriptions`, { method: "POST", body: form });
    const text = await r.text();
    if (![429, 503].includes(r.status) || attempt >= 120) { let json = {}; try { json = JSON.parse(text); } catch { /* not JSON */ } return { ok: r.ok, text: json.text ?? "" }; }
    await sleep(15_000);
  }
}

// ---------- vision: drawn images with known answers ----------
const svg = (body, width = 360, height = 260) => `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" style="background:#fff">${body}</svg>`;
const bars = (values, labels) => svg(values.map((value, i) => `<rect x="${40 + i * 70}" y="${220 - value}" width="50" height="${value}" fill="#2563eb"/><text x="${58 + i * 70}" y="245" font-size="20">${labels[i]}</text>`).join(""));
const VISION = [
  { id: "V01", html: svg('<circle cx="180" cy="130" r="80" fill="#dc2626"/>'), q: "What colour and shape is this?", expect: [/red/i, /circle/i] },
  { id: "V02", html: svg('<rect x="90" y="40" width="180" height="180" fill="#2563eb"/>'), q: "What colour and shape is this?", expect: [/blue/i, /square/i] },
  { id: "V03", html: svg('<polygon points="180,30 320,230 40,230" fill="#16a34a"/>'), q: "What colour and shape is this?", expect: [/green/i, /triangle/i] },
  { id: "V04", html: svg('<circle cx="70" cy="130" r="40" fill="#ea580c"/><circle cx="180" cy="130" r="40" fill="#ea580c"/><circle cx="290" cy="130" r="40" fill="#ea580c"/>'), q: "How many circles are there?", expect: [/\b(3|three)\b/i] },
  { id: "V05", html: svg([20, 90, 160, 230, 300].map((x, i) => `<rect x="${x}" y="${i % 2 ? 40 : 160}" width="50" height="50" fill="#7c3aed"/>`).join(""), 380), q: "How many squares are there?", expect: [/\b(5|five)\b/i] },
  { id: "V06", html: bars([60, 110, 200, 90], ["A", "B", "C", "D"]), q: "Which label has the tallest bar?", expect: [/\bC\b/] },
  { id: "V07", html: svg('<circle cx="180" cy="130" r="100" fill="#2563eb"/><path d="M180 130 L180 30 A100 100 0 0 1 280 130 Z" fill="#facc15"/>'), q: "Which colour takes up most of this pie chart?", expect: [/blue/i] },
  { id: "V08", html: svg('<polyline points="30,230 100,190 170,150 240,90 330,30" fill="none" stroke="#dc2626" stroke-width="6"/>'), q: "Does this line chart go up or down from left to right?", expect: [/\bup\b|ris|increas|upward/i] },
  { id: "V09", html: '<h2>Sign in</h2><label>Email</label><br><input value="dev@example.com"><br><label>Password</label><br><input type="password" value="xxxxxxxx"><br><br><button style="background:#2563eb;color:#fff;padding:8px 24px">Log in</button>', q: "What kind of screen is this?", expect: [/log.?in|sign.?in/i] },
  { id: "V10", html: '<table border="1" cellpadding="8"><tr><th>Name</th><th>Role</th></tr><tr><td>Ana</td><td>Engineer</td></tr><tr><td>Ben</td><td>Designer</td></tr><tr><td>Chen</td><td>Manager</td></tr></table>', q: "How many people are listed in this table?", expect: [/\b(3|three)\b/i] },
  { id: "V11", html: '<pre style="background:#111;color:#f87171;padding:12px;font:18px Menlo">error[E0308]: mismatched types\n  --> src/main.rs:12:5</pre>', q: "Does this terminal show a success or a failure?", expect: [/fail|error/i] },
  { id: "V12", html: '<div style="background:#16a34a;color:#fff;padding:16px;font:22px sans-serif;border-radius:8px;width:300px">&#10003; Saved successfully</div>', q: "Does this message report a success or a failure?", expect: [/success|saved/i] },
  { id: "V13", html: svg('<rect x="50" y="110" width="170" height="40" fill="#111"/><polygon points="220,70 320,130 220,190" fill="#111"/>'), q: "Which way does this arrow point: left, right, up or down?", expect: [/right/i] },
  { id: "V14", html: svg('<rect x="130" y="10" width="100" height="240" rx="14" fill="#222"/><circle cx="180" cy="55" r="30" fill="#ef4444"/><circle cx="180" cy="130" r="30" fill="#444"/><circle cx="180" cy="205" r="30" fill="#444"/>'), q: "Which light of this traffic light is on?", expect: [/red|top/i] },
  { id: "V15", html: svg('<circle cx="180" cy="130" r="100" fill="#facc15"/><circle cx="145" cy="105" r="12" fill="#111"/><circle cx="215" cy="105" r="12" fill="#111"/><path d="M125 150 Q180 210 235 150" stroke="#111" stroke-width="8" fill="none"/>'), q: "What does this drawing show?", expect: [/smil|happy|face|emoji/i] },
  { id: "V16", html: svg('<polygon points="180,30 320,130 40,130" fill="#b91c1c"/><rect x="80" y="130" width="200" height="110" fill="#92400e"/><rect x="155" y="170" width="50" height="70" fill="#fde68a"/>'), q: "What does this drawing show?", expect: [/house|home|building/i] },
  { id: "V17", html: '<div style="font:24px sans-serif"><span style="display:inline-block;width:28px;height:28px;border:3px solid #111;text-align:center;line-height:28px">&#10003;</span> Accept terms</div>', q: "Is the checkbox ticked? Answer yes or no.", expect: [/\byes\b/i] },
  { id: "V18", html: '<div style="background:#111;color:#eee;padding:30px;font:22px sans-serif;width:320px">Settings<br><small>Notifications are on</small></div>', q: "Is this a dark theme or a light theme?", expect: [/dark/i] },
  { id: "V19", html: '<div style="width:320px;height:36px;border:2px solid #111"><div style="width:75%;height:100%;background:#16a34a"></div></div>', q: "About how full is this progress bar, as a percentage?", expect: [/\b(7\d|8\d)\b|three.quarter/i] },
  { id: "V20", html: svg('<rect width="360" height="260" fill="#e0f2fe"/><path d="M20 240 C 120 20, 240 240, 340 30" stroke="#ea580c" stroke-width="6" fill="none"/><circle cx="20" cy="240" r="11" fill="#dc2626"/><circle cx="340" cy="30" r="11" fill="#16a34a"/>'), q: "What does this picture show: a map route, a bar chart, or a table?", expect: [/route|map/i] },
];
function visionImage(c) {
  const base = path.join(WORK, c.id);
  if (!existsSync(`${base}.png`)) {
    writeFileSync(`${base}.html`, `<html><body style="margin:16px;background:#fff;font-family:sans-serif">${c.html}</body></html>`);
    execFileSync(CHROME, ["--headless=new", "--disable-gpu", "--hide-scrollbars", `--screenshot=${base}.png`, "--window-size=480,320", `file://${base}.html`], { stdio: "ignore" });
  }
  return readFileSync(`${base}.png`).toString("base64");
}
async function vision(model, capabilities) {
  const rows = {}; const times = [];
  const think = capabilities.includes("thinking") ? { think: false } : {};
  for (let round = 0; round < ROUNDS; round++) for (const c of VISION) {
    const r = await asked("/tasks", { task: "vision", model, prompt: `${c.q} Answer briefly.`, images: [visionImage(c)], max_wait_seconds: 120, request_options: { ...think, options: { num_predict: 200 } } });
    const text = r.json?.response?.message?.content ?? "";
    (rows[c.id] ??= []).push(r.status === 200 && c.expect.every((fact) => fact.test(text)));
    if (round > 0) times.push(r.ms);
    if (r.status !== 200) log(`${model} ${c.id}: ${r.status} ${JSON.stringify(r.json).slice(0, 160)}`);
  }
  return summarize(rows, times);
}

// ---------- image search and code search ----------
const CODE_QUERIES = [
  ["how a decision task request is built for /v1/systemone", "execution/request.rs"],
  ["which programs hold the most memory when a model load has to wait", "resources/holders.rs"],
  ["unload idle models when the host is under memory pressure", "sweep.rs"],
  ["learn how many bytes per token a model uses", "model_facts.rs"],
  ["refuse an embedding item too long for the context window", "execution/request.rs"],
  ["stop a streaming answer once it starts repeating itself", "repetition.rs"],
  ["how long to keep a model loaded after use, adaptive keep alive", "warming.rs"],
  ["run a batch of independent tasks with priority-fair dispatch", "tasks_api.rs"],
  ["hold new model loads while the system is swapping", "resources/mod.rs"],
  ["keep related tasks on the same model with session affinity", "routing/session.rs"],
];
const PREFIXES = { "nomic-embed-text:v1.5": ["search_query: ", "search_document: "], "embeddinggemma-2:740m-q8_0": ["task: search result | query: ", "title: none | text: "] };
function codeChunks() {
  const chunks = [];
  for (const relative of globSync("packages/rust-core/src/platform/**/*.rs", { cwd: ROOT }).sort()) {
    const lines = readFileSync(path.join(ROOT, relative), "utf8").split("\n");
    let current = [], bytes = 0;
    const flush = () => { if (current.join("").trim()) chunks.push({ file: relative, text: `${relative}\n${current.join("\n")}` }); };
    for (const line of lines) {
      if (bytes + line.length + 1 > 1536 && current.length) { flush(); current = current.slice(-8); bytes = current.join("\n").length; }
      current.push(line.slice(0, 1400)); bytes += Math.min(line.length, 1400) + 1;
    }
    flush();
  }
  return chunks;
}
const cosine = (a, b) => { let d = 0, x = 0, y = 0; for (let i = 0; i < a.length; i++) { d += a[i] * b[i]; x += a[i] ** 2; y += b[i] ** 2; } return d / Math.sqrt(x * y); };
async function embedAll(model, texts) {
  const vectors = [];
  for (let at = 0; at < texts.length; at += 64) {
    const r = await asked("/tasks", { task: "embedding", model, input: texts.slice(at, at + 64), max_wait_seconds: 120 });
    if (r.status !== 200) throw new Error(`embed: ${r.status} ${JSON.stringify(r.json).slice(0, 200)}`);
    vectors.push(...r.json.response.embeddings);
  }
  return vectors;
}
async function search(model) {
  const [queryPrefix, documentPrefix] = PREFIXES[model];
  const chunks = codeChunks();
  const t0 = performance.now();
  const vectors = await embedAll(model, chunks.map((chunk) => documentPrefix + chunk.text));
  const index_s = Number(((performance.now() - t0) / 1000).toFixed(1));
  let top1 = 0, top3 = 0; const misses = [];
  const queries = await embedAll(model, CODE_QUERIES.map(([query]) => queryPrefix + query));
  CODE_QUERIES.forEach(([query, expected], i) => {
    const best = new Map();
    chunks.forEach((chunk, j) => { const score = cosine(queries[i], vectors[j]); if (score > (best.get(chunk.file) ?? -2)) best.set(chunk.file, score); });
    const ranked = [...best.entries()].sort((a, b) => b[1] - a[1]).map(([file]) => file);
    const at = ranked.findIndex((file) => file.endsWith(expected));
    if (at === 0) top1++;
    if (at >= 0 && at < 3) top3++; else misses.push(`${expected} (rank ${at + 1})`);
    void query;
  });
  // Embeddings are deterministic, so one round is the result.
  return { total: CODE_QUERIES.length, top1, top3, chunks: chunks.length, index_s, misses };
}

// ---------- coding ----------
async function coding(model, capabilities) {
  const rows = {}; const times = [];
  const think = capabilities.includes("thinking") ? { think: false } : {};
  for (let round = 0; round < ROUNDS; round++) for (const c of CASES.code) {
    const r = await asked("/tasks", { task: c.task ?? "coding", model, context_tokens: 4096, max_wait_seconds: 120,
      messages: [{ role: "system", content: "Answer with only what is asked: no explanation, no code fences unless asked." }, { role: "user", content: c.prompt }],
      request_options: { ...think, options: { num_predict: 800 } } });
    const answer = r.json?.response?.message?.content ?? "";
    (rows[c.id] ??= []).push(r.status === 200 && (c.expect instanceof RegExp ? c.expect.test(answer) : norm(answer).includes(norm(c.expect))));
    if (round > 0) times.push(r.ms);
    if (r.status !== 200) log(`${model} ${c.id}: ${r.status} ${JSON.stringify(r.json).slice(0, 160)}`);
  }
  return summarize(rows, times);
}

// ---------- research agent with Octocode ----------
function agent(model) {
  const report = path.join(WORK, `agent-${model.replace(/\W+/g, "-")}.md`);
  rmSync(report, { force: true });
  const runRound = () => new Promise((resolve) => {
    const child = spawn("node", [path.join(ROOT, "scripts/local-test/octocode-agent.mjs"), model, SERVE, "all"], { cwd: ROOT, stdio: ["ignore", "ignore", "ignore"],
      env: { ...process.env, FREELLAMA_AGENT_REPORT: report, FREELLAMA_ROOM_WAIT_MS: "7200000", FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS: "400" } });
    child.on("close", resolve);
  });
  return (async () => {
    for (let round = 0; round < ROUNDS; round++) { log(`${model}: agent round ${round + 1}`); await runRound(); }
    const text = readFileSync(report, "utf8");
    const rows = {}; const calls = []; const seconds = [];
    for (const match of text.matchAll(/^\| ([LG]\d) \|[^|]*\| (✅|❌) \| (\d+) \|[^|]*\|[^|]*\| (\d+) \|/gm)) {
      (rows[match[1]] ??= []).push(match[2] === "✅");
      calls.push(Number(match[3])); seconds.push(Number(match[4]) * 1000);
    }
    const ids = Object.keys(rows);
    const count = (prefix) => Array.from({ length: ROUNDS }, (_, round) => ids.filter((id) => id.startsWith(prefix) && rows[id][round]).length);
    return { total: ids.length, local: count("L"), github: count("G"), flaky: ids.filter((id) => new Set(rows[id]).size > 1), calls_per_question: Number((calls.reduce((a, b) => a + b, 0) / Math.max(1, calls.length)).toFixed(1)), median_ms: Math.round(median(seconds)) };
  })();
}

// ---------- cookbooks: run each recipe's calls as written, on real files ----------
const COOKBOOKS = path.join(ROOT, "docs/cookbooks");
function recipeFixtures() {
  const dir = path.join(WORK, "fixtures");
  for (const sub of ["issues", "shots", "releases"]) mkdirSync(path.join(dir, sub), { recursive: true });
  const texts = { "issues/a.md": "Crash when saving a file with a long path.", "issues/b.md": "Add a dark theme to the settings page.", "issues/c.md": "Saving a file with a very long path makes the app crash.", "releases/v1.md": "Version 1 adds login and fixes a crash on save.", "releases/v2.md": "Version 2 adds dark mode and speeds up search." };
  for (const [name, text] of Object.entries(texts)) writeFileSync(path.join(dir, name), text);
  for (const id of ["V06", "V09", "V11"]) { const c = VISION.find((item) => item.id === id); writeFileSync(path.join(dir, "shots", `${id}.png`), Buffer.from(visionImage(c), "base64")); }
  return { "issues/a.md": path.join(dir, "issues/a.md"), "issues/b.md": path.join(dir, "issues/b.md"), "issues/*.md": path.join(dir, "issues/*.md"), "src/**/*.rs": "packages/rust-core/src/platform/resources/*.rs",
    "screenshots/error.png": (ocrImage(CASES.ocr[0]), path.join(WORK, `${CASES.ocr[0].id}.png`)), "charts/usage.png": path.join(WORK, "V06.png"), "screenshots/*.png": path.join(dir, "shots/*.png"),
    "recordings/standup.wav": path.join(WORK, "s1.wav"), "reply.wav": path.join(WORK, "reply.wav"), "src/parse.ts": "packages/mcp/src/nearest.ts", "releases/v1.md": path.join(dir, "releases/v1.md"), "releases/v2.md": path.join(dir, "releases/v2.md") };
}
const recipeCalls = (name) => [...readFileSync(path.join(COOKBOOKS, `${name}.md`), "utf8").matchAll(/\`\`\`json\n(.*?)\n\`\`\`/gs)].map((match) => JSON.parse(match[1]));
const recipeModels = (name) => [...new Set(recipeCalls(name).map((call) => call.arguments.model).filter(Boolean))];
const ENDPOINT_FIELD = { doctor: "serveEndpoint" };
async function recipe(name) {
  await speakAll();
  const fixtures = recipeFixtures();
  const mcp = spawn("node", [path.join(ROOT, "packages/mcp/dist/index.js")], { cwd: ROOT, stdio: ["pipe", "pipe", "ignore"], env: { ...process.env, FREELLAMA_SERVE_ENDPOINT: SERVE, FREELLAMA_MCP_AUTOSTART_SERVE: "0", FREELLAMA_MCP_ALLOWED_ROOTS: ROOT, FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS: "400" } });
  let buffer = ""; const waiting = new Map(); let id = 0;
  mcp.stdout.on("data", (chunk) => { buffer += chunk; let at; while ((at = buffer.indexOf("\n")) >= 0) { const line = buffer.slice(0, at); buffer = buffer.slice(at + 1); try { const message = JSON.parse(line); waiting.get(message.id)?.(message); } catch { /* not JSON */ } } });
  const rpc = (method, params) => new Promise((resolve) => { const key = ++id; waiting.set(key, resolve); mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: key, method, params })}\n`); });
  await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "recipes", version: "1" } });
  const calls = [];
  try {
    for (const call of recipeCalls(name)) {
      let text = JSON.stringify(call.arguments);
      for (const [from, to] of Object.entries(fixtures)) text = text.replaceAll(JSON.stringify(from), JSON.stringify(to));
      const args = JSON.parse(text);
      if (args.workspacePath === ".") args.workspacePath = ROOT;
      args[ENDPOINT_FIELD[call.tool] ?? "endpoint"] = SERVE;
      let reply; let body = "";
      for (let attempt = 0; attempt < 80; attempt++) {
        reply = (await rpc("tools/call", { name: call.tool, arguments: args })).result ?? { isError: true, content: [{ text: "no result" }] };
        body = reply.content?.map((part) => part.text ?? "").join(" ") ?? "";
        if (!reply.isError || !/admission|memory|busy/i.test(body)) break;
        await sleep(15_000);
      }
      calls.push({ tool: call.tool, ok: !reply.isError && body.trim().length > 0, text: body.replace(/\s+/g, " ").slice(0, 110) });
    }
  } finally {
    mcp.kill();
    for (const model of recipeModels(name)) await unload(model);
  }
  return { total: calls.length, passed: calls.filter((call) => call.ok).length, calls };
}

let catalog = [];
// ---------- one model's profile: context window, memory, caching, speed ----------
const PREFIX = Array.from({ length: 90 }, (_, i) => `Rule ${i + 1}: requests for module ${i * 7 % 13} must be logged with their trace id and retried at most ${i % 5 + 1} times.`).join("\n");
const OVERSIZED = "word ".repeat(40_000);
const num = (value, digits = 0) => (typeof value === "number" ? Number(value.toFixed(digits)) : null);
async function residentEntry(model) {
  const models = (await http(`${SERVE}/api/ps`)).json?.models ?? [];
  const entry = models.find((m) => m.name === model || m.name === `${model}:latest`);
  if (!entry) return null;
  return { resident_gb: num(entry.size / 1e9, 2), vram_gb: num(entry.size_vram / 1e9, 2), context: entry.context_length ?? null, keep_alive_s: entry.expires_at ? Math.round((Date.parse(entry.expires_at) - Date.now()) / 1000) : null };
}
async function profile(model, capabilities) {
  await unload(model);
  const facts = catalog.find((entry) => entry.name === model) ?? {};
  const base = { type: facts.model_type, advertised_context: facts.advertised_context ?? null, modelfile_context: facts.modelfile_num_ctx ?? null, file_gb: num((facts.size ?? 0) / 1e9, 1) };
  if (facts.model_type === "speech") {
    const t0 = performance.now();
    const r = await fetch(`${SERVE}/v1/audio/speech`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ model, input: "The build passed.", response_format: "wav" }) });
    await r.arrayBuffer();
    return { ...base, status: r.status, cold_ms: Math.round(performance.now() - t0), ...await residentEntry(model) };
  }
  if (facts.model_type === "embedding_only") {
    const items = Array.from({ length: 32 }, (_, i) => `Rule ${i}: requests for module ${i} must be logged with their trace id.`);
    const cold = await asked("/tasks", { task: "embedding", model, input: items, max_wait_seconds: 120 });
    const warm = await asked("/tasks", { task: "embedding", model, input: items, max_wait_seconds: 120 });
    const oversized = await http("/tasks", { task: "embedding", model, input: [OVERSIZED], max_wait_seconds: 30 });
    return { ...base, load_s: num((cold.json?.metrics?.load_duration_ns ?? 0) / 1e9, 1), warm_items_per_s: num(items.length / (warm.ms / 1000), 0), oversized: `${oversized.status} ${oversized.json?.code ?? ""}`.trim(), ...await residentEntry(model) };
  }
  if (facts.model_type === "decision") {
    const ask = (question) => asked("/tasks", { task: "decision", model, state: PREFIX, questions: { q: { type: "noul", instructions: question } }, max_wait_seconds: 120 });
    const first = await ask("Do the rules mention trace ids?");
    const second = await ask("Do the rules mention retries?");
    const oversized = await http("/tasks", { task: "decision", model, state: OVERSIZED, questions: { q: { type: "noul", instructions: "Is this repetitive?" } }, max_wait_seconds: 30 });
    return { ...base, first_ms: Math.round(first.ms), load_s: num((first.json?.metrics?.load_duration_ns ?? 0) / 1e9, 1), prompt_tokens: second.json?.metrics?.prompt_tokens ?? null, cached_tokens: second.json?.response?.prompt_eval_cached_count ?? second.json?.metrics?.cached_prompt_tokens ?? null, second_ms: Math.round(second.ms), oversized: `${oversized.status} ${String(oversized.json?.code ?? oversized.json?.error ?? "").slice(0, 60)}`.trim(), ...await residentEntry(model) };
  }
  const think = capabilities.includes("thinking") ? { think: false } : {};
  const chat = (question, context, predict, system = PREFIX) => asked("/tasks", { task: "completion", model, context_tokens: context, max_wait_seconds: 180, stop_on_repetition: false,
    messages: [{ role: "system", content: system }, { role: "user", content: question }], request_options: { ...think, options: { num_predict: predict } } });
  const small = 8192;
  const first = await chat("How many rules are there?", small, 16);
  const second = await chat("Which rule mentions module 3?", small, 16);
  const output = await chat("Count from 1 to 60, separated by spaces.", small, 128);
  const memory = {};
  memory[small] = (await residentEntry(model))?.resident_gb ?? null;
  const facts8k = await residentEntry(model);
  const contexts = {};
  for (const context of [16_384, 65_536]) {
    if ((facts.advertised_context ?? 0) < context) { contexts[context] = "over its maximum"; continue; }
    const r = await http("/tasks", { task: "completion", model, context_tokens: context, max_wait_seconds: 60, messages: [{ role: "user", content: "Say OK." }], request_options: { ...think, options: { num_predict: 4 } } });
    const entry = await residentEntry(model);
    contexts[context] = r.status === 200 ? `${entry?.resident_gb ?? "?"} GB${(r.json?.metrics?.load_duration_ns ?? 0) > 1e9 ? " (reloaded)" : ""}` : `held (${r.status})`;
  }
  const oversized = await http("/tasks", { task: "completion", model, context_tokens: small, max_wait_seconds: 30, messages: [{ role: "user", content: `${OVERSIZED}\nSay OK.` }], request_options: { ...think, options: { num_predict: 4 } } });
  return { ...base, load_s: num((first.json?.metrics?.load_duration_ns ?? 0) / 1e9, 1), resident_8k: memory[small], context_16k: contexts[16_384], context_64k: contexts[65_536], keep_alive_s: facts8k?.keep_alive_s ?? null,
    prompt_tokens: first.json?.metrics?.prompt_tokens ?? null, first_prompt_ms: num((first.json?.metrics?.prompt_duration_ns ?? 0) / 1e6, 0), cached_tokens: second.json?.metrics?.cached_prompt_tokens ?? null, repeat_prompt_ms: num((second.json?.metrics?.prompt_duration_ns ?? 0) / 1e6, 0),
    output_tps: num(output.json?.metrics?.output_tokens_per_second, 0), oversized: `${oversized.status} ${String(oversized.json?.code ?? "").slice(0, 40)}`.trim() };
}
const gateTask = (model) => { const type = catalog.find((entry) => entry.name === model)?.model_type; return type === "embedding_only" ? "embedding" : type === "decision" ? "decision" : "completion"; };

const GROUPS = [
  { key: "classify", title: "Classify and triage", task: "decision", note: `${CLASSIFY.length} questions: 20 decision questions plus ${CLASSIFY.length - 20} real labeled issues.`, models: ["strands-decider:2b-nvfp4", "nimble-v3:9b-q4_K_M"], run: classify },
  { key: "ocr", title: "OCR", task: "vision", note: `${CASES.ocr.length} rendered text images; right means every expected phrase is present. Same prompt for both, as each model's card documents.`, models: ["glm-ocr:latest", "hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M"], run: ocr },
  { key: "speech", title: "Speech to text", task: "completion", note: `${SENTENCES.length} sentences spoken by phonon:en; "forty two" and "42" count as equal. Generated speech, not human recordings.`, models: ["lfm3-audio:2b-q4_K_M", "lfm3-omni:2b-q4_K_M"], run: speech },
  { key: "tts", title: "Text to speech", task: "completion", gate: SPEECH_JUDGE, note: `${SENTENCES.length} sentences spoken, then transcribed by ${SPEECH_JUDGE}. This scores clarity to a machine, not how natural it sounds; the judge may favor its own voice.`, models: ["phonon:en", "lfm3-audio:2b-q4_K_M", "lfm3-omni:2b-q4_K_M"], run: textToSpeech },
  { key: "vision", title: "Image recognition", task: "vision", note: `${VISION.length} drawn images with known answers: shapes, counts, charts, screens. Drawings, not photos, so this says little about real-world pictures.`, models: ["lfm3-vl:2b-q4_K_M", "lfm3-omni:2b-q4_K_M", "bonsai-2:9b", "bonsai-2:27b"], run: vision },
  { key: "search", title: "Code search", task: "embedding", note: `${CODE_QUERIES.length} questions over FreeLlama's platform source; one round, because embeddings are deterministic.`, models: ["nomic-embed-text:v1.5", "embeddinggemma-2:740m-q8_0"], run: search },
  { key: "coding", title: "Coding", task: "coding", note: `${CASES.code.length} small coding questions with known answers.`, models: ["lfm3:1.7b-q4_K_M", "bonsai-2:9b", "bonsai-2:27b", NEMOTRON], run: coding },
  { key: "agent", title: "Research agent with Octocode", task: "coding", note: "8 local and 6 GitHub questions; right means every expected fact and at least one tool call.", models: ["lfm3-audio:2b-q4_K_M", "lfm3:1.7b-q4_K_M", "bonsai-2:9b", "bonsai-2:27b", NEMOTRON], run: agent },
  { key: "recipes", title: "Cookbook recipes, run as written", note: "Each recipe's calls ran through the real MCP server on real files, with placeholder paths replaced. Passing means the call succeeded and returned something, not that the answer is right; the sections above score quality.", models: readdirSync(COOKBOOKS).filter((name) => name.endsWith(".md") && name !== "README.md").map((name) => name.replace(/\.md$/, "")), run: recipe },
  { key: "profile", title: "Model profiles: context, memory, caching, speed", note: "One run per model, one at a time, with a repeated 90-rule prefix of about 2,000 tokens. Timings are noisy on a busy machine.", models: [], run: profile },
];

function writeReport() {
  const out = ["# LOCAL_RIVALS: the same questions on rival models", "",
    `Three rounds per model unless noted, one model at a time, through FreeLlama at ${SERVE}. Apple M2 Pro, 32 GB, Ollama 0.40.13, with other work running; timings are noisy, so compare accuracy first. Questions were written before these runs, by the same author as the harness. Small sets on one machine: a difference of one or two questions is not a real difference.`, ""];
  for (const group of GROUPS) {
    out.push(`## ${group.title}`, "", group.note, "");
    if (group.key === "recipes") {
      out.push("| Recipe | Calls passed | First answer | Failures |", "|---|---:|---|---|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; const failed = r?.calls.filter((call) => !call.ok) ?? []; out.push(r ? `| ${model} | ${r.passed}/${r.total} | ${(r.calls[0]?.text ?? "").replace(/\|/g, "/")} | ${failed.map((call) => `${call.tool}: ${call.text}`).join("; ").replace(/\|/g, "/") || "none"} |` : `| ${model} | waiting | | |`); }
    } else if (group.key === "profile") {
      out.push("| Model | Type | File GB | Max context | Load s | Resident GB at 8K | At 16K | At 64K | Keep-alive s | Prompt tokens | First prompt ms | Cached on repeat | Repeat ms | Output tok/s | Oversized input |", "|---|---|---:|---:|---:|---:|---|---|---:|---:|---:|---:|---:|---:|---|");
      for (const model of group.models) {
        const r = results[`${group.key}:${model}`];
        const cell = (value) => value ?? "—";
        out.push(r ? `| \`${model}\` | ${cell(r.type)} | ${cell(r.file_gb)} | ${cell(r.advertised_context)} | ${cell(r.load_s)} | ${cell(r.resident_8k ?? r.resident_gb)} | ${cell(r.context_16k)} | ${cell(r.context_64k)} | ${cell(r.keep_alive_s)} | ${cell(r.prompt_tokens)} | ${cell(r.first_prompt_ms ?? r.first_ms)} | ${cell(r.cached_tokens)} | ${cell(r.repeat_prompt_ms ?? r.second_ms)} | ${cell(r.output_tps ?? r.warm_items_per_s)} | ${cell(r.oversized)} |` : `| \`${model}\` | waiting | | | | | | | | | | | | | |`);
      }
    } else if (group.key === "tts") {
      out.push("| Model | Word error, each round | Clean sentences, each round | Median ms | Average audio |", "|---|---|---|---:|---:|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; out.push(r ? `| \`${model}\` | ${r.rounds.map((round) => `${round.wer}%`).join(", ")} | ${r.rounds.map((round) => round.perfect).join(", ")} of ${r.total} | ${r.median_ms} | ${r.audio_seconds} s |` : `| \`${model}\` | waiting | | | |`); }
    } else if (group.key === "search") {
      out.push("| Model | Right file first | Right file in top 3 | Chunks | Index time | Missed |", "|---|---:|---:|---:|---:|---|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; out.push(r ? `| \`${model}\` | ${r.top1}/${r.total} | ${r.top3}/${r.total} | ${r.chunks} | ${r.index_s} s | ${r.misses.join(", ") || "none"} |` : `| \`${model}\` | waiting | | | | |`); }
    } else if (group.key === "agent") {
      out.push("| Model | Local, each round (of 8) | GitHub, each round (of 6) | Changed between rounds | Calls per question | Median s |", "|---|---|---|---|---:|---:|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; out.push(r ? `| \`${model}\` | ${r.local.join(", ")} | ${r.github.join(", ")} | ${r.flaky.join(", ") || "none"} | ${r.calls_per_question} | ${(r.median_ms / 1000).toFixed(0)} |` : `| \`${model}\` | waiting | | | | |`); }
    } else if (group.key === "speech") {
      out.push("| Model | Word error | Perfect sentences, each round | Median ms | French | Spanish | Sample misses |", "|---|---:|---|---:|---|---|---|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; out.push(r ? `| \`${model}\` | ${r.wer}% | ${r.perRound.join(", ")} of ${r.total} | ${r.median_ms} | ${r.translate.ok_fr ? "✅" : "❌"} ${r.translate.fr.replace(/\|/g, "/")} | ${r.translate.ok_es ? "✅" : "❌"} ${r.translate.es.replace(/\|/g, "/")} | ${r.samples.join(" / ").replace(/\|/g, "/") || "none"} |` : `| \`${model}\` | waiting | | | | | |`); }
    } else {
      out.push("| Model | Right, each round | Changed between rounds | Median ms | Usually wrong |", "|---|---|---|---:|---|");
      for (const model of group.models) { const r = results[`${group.key}:${model}`]; out.push(r ? `| \`${model}\` | ${r.perRound.join(", ")} of ${r.total} | ${r.flaky.length} | ${r.median_ms} | ${r.misses.join(", ") || "none"} |` : `| \`${model}\` | waiting | | | |`); }
    }
    out.push("");
  }
  writeFileSync(REPORT, `${out.join("\n")}\n`);
}

// Run whichever pending model has room now, smaller groups first, one at a time.
catalog = (await http("/models")).json?.models ?? [];
GROUPS.find((group) => group.key === "profile").models = catalog.map((entry) => entry.name).sort((a, b) => (catalog.find((m) => m.name === a).size - catalog.find((m) => m.name === b).size));
const capabilitiesOf = (model) => catalog.find((entry) => entry.name === model)?.capabilities ?? [];
const pending = GROUPS.flatMap((group) => group.models.map((model) => ({ group, model }))).filter(({ group, model }) => !results[`${group.key}:${model}`]);
const started = Date.now();
writeReport();
while (pending.length > 0 && Date.now() - started < LIMIT_MS) {
  let ran = false;
  for (const entry of [...pending]) {
    const speechModel = catalog.find((m) => m.name === entry.model)?.model_type === "speech";
    const recipeGate = entry.group.key === "recipes" ? recipeModels(entry.model).find((model) => model !== "phonon:en") ?? null : undefined;
    const gate = recipeGate !== undefined ? recipeGate : entry.group.key === "profile" ? (speechModel ? SPEECH_JUDGE : entry.model) : entry.group.gate ?? entry.model;
    if (gate !== null && !await admitted(entry.group.key === "profile" || entry.group.key === "recipes" ? gateTask(gate) : entry.group.task, gate)) continue;
    const before = await hostLoad();
    log(`${entry.group.key}: ${entry.model} (${before.available_gb} GB available, load ${before.load})`);
    try {
      const result = await entry.group.run(entry.model, capabilitiesOf(entry.model));
      results[`${entry.group.key}:${entry.model}`] = { ...result, host: before };
      writeFileSync(STORE, JSON.stringify(results, null, 2));
    } catch (error) {
      log(`${entry.model} failed: ${error.message}`);
      results[`${entry.group.key}:${entry.model}:error`] = String(error.message);
    }
    if (entry.group.key !== "recipes") await unload(entry.model);
    pending.splice(pending.indexOf(entry), 1);
    writeReport();
    ran = true;
    break;
  }
  if (!ran) { log(`waiting for room: ${pending.map((entry) => entry.model).join(", ")}`); await sleep(30_000); }
}
writeReport();
log(pending.length > 0 ? `stopped with ${pending.length} waiting` : "done");
process.exit(0);
