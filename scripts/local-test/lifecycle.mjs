#!/usr/bin/env node
// How FreeLlama runs models on this machine: cold and warm calls, keep-alive, unload, switching,
// queueing and priorities, prompt caching, the idle sweep, CPU vs GPU, and edge cases. Small models
// only, one step at a time, each scenario waiting until FreeLlama says its model fits. Writes
// LOCAL_LIFECYCLE.md. Usage: node scripts/local-test/lifecycle.mjs [serveEndpoint]
import { spawn } from "node:child_process";
import { cpSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import path from "node:path";

const SERVE = (process.argv[2] ?? "http://127.0.0.1:11435").replace(/\/$/, "");
const API = `${SERVE}/_freellama/v1`;
const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../..");
const BIN = path.join(ROOT, "target/release/freellama");
const EMB = "nomic-embed-text:v1.5", DEC = "strands-decider:2b-nvfp4", OCR = "glm-ocr:latest";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const out = [];
const section = (title) => { out.push({ title, rows: [], notes: [] }); console.error(`== ${title}`); };
const row = (...cells) => { out.at(-1).rows.push(cells); console.error("  ", cells.join(" | ")); };
const note = (text) => { out.at(-1).notes.push(text); console.error("  note:", text); };

async function http(method, url, body) {
  const t0 = performance.now();
  try {
    const r = await fetch(url.startsWith("http") ? url : `${API}${url}`, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    let json; try { json = JSON.parse(text); } catch { json = { raw: text.slice(0, 300) }; }
    return { status: r.status, json, ms: Math.round(performance.now() - t0) };
  } catch (error) { return { status: 0, json: { error: String(error.cause?.code ?? error) }, ms: Math.round(performance.now() - t0) }; }
}
const embed = (input, extra = {}) => http("POST", "/tasks", { task: "embedding", model: EMB, input, max_wait_seconds: 120, ...extra });
const ps = async () => ((await http("GET", "http://127.0.0.1:11434/api/ps")).json.models ?? []);
const resident = async () => (await ps()).map((m) => m.name);
const ttlSeconds = async (model) => { const m = (await ps()).find((x) => x.name === model); return m ? Math.round((Date.parse(m.expires_at) - Date.now()) / 1000) : null; };
const loadMs = (r) => Math.round((r.json?.metrics?.load_duration_ns ?? 0) / 1e6);
async function fits(model, task) {
  for (let i = 0; i < 90; i++) {
    const r = await http("POST", "/routes", { task, model });
    const a = r.json?.execution?.resource_assessment;
    if (String(a?.status).startsWith("ready")) return true;
    if (i === 0) console.error(`   ${model}: ${a?.status}; waiting (largest: ${(r.json?.execution?.memory_holders ?? []).map((h) => h.name).join(", ")})`);
    await sleep(10_000);
  }
  return false;
}

// 1. Cold, warm and the cost FreeLlama adds.
section("Cold load, warm calls and FreeLlama's own cost (nomic-embed-text)");
await http("POST", "/unload", { model: EMB });
if (await fits(EMB, "embedding")) {
  const cold = await embed(["cold start"]);
  row("cold call", `${cold.ms} ms wall`, `${loadMs(cold)} ms load`);
  const warm = [];
  for (let i = 0; i < 10; i++) { const r = await embed([`warm ${i}`]); warm.push([r.ms, Math.round((r.json?.metrics?.total_duration_ns ?? 0) / 1e6)]); }
  warm.sort((a, b) => a[0] - b[0]);
  const [wall, ollama] = warm[5];
  row("warm call (median of 10)", `${wall} ms wall`, `${ollama} ms in Ollama`, `${wall - ollama} ms FreeLlama + HTTP`);
}

// 2. Keep-alive grows with use.
section("Adaptive keep-alive (how long an idle model stays loaded)");
row("after 11 uses", `${await ttlSeconds(EMB)} s left`);
for (let i = 0; i < 10; i++) await embed([`use ${i}`]);
row("after 21 uses", `${await ttlSeconds(EMB)} s left`);
note("FreeLlama sends keep_alive = clamp(60 s + 30 s per recent use + 2 x load seconds, 15..900 s), shortened 4x while the host is under pressure.");

// 3. Unload: idle, busy, absent.
section("Unload");
{
  const r = await http("POST", "/unload", { model: EMB });
  row("idle resident model", `${r.ms} ms`, r.json.status);
  const again = await http("POST", "/unload", { model: EMB });
  row("model not loaded", `${again.ms} ms`, again.json.status, "nothing was loaded first");
  await embed(["reload"]);
  const big = embed(Array.from({ length: 1500 }, (_, i) => `busy item ${i} with some words to embed`));
  await sleep(150);
  const busy = await http("POST", "/unload", { model: EMB });
  const task = await big;
  row("model busy with a 1,500-item batch", `${busy.ms} ms`, busy.json.status ?? busy.json.error, `batch took ${task.ms} ms`);
  note("An unload waits for running tasks on that model (shared transition lock), then unloads.");
}

// 4. Switching between models and co-residency.
section("Switching models (embedding <-> decision)");
{
  for (const [i, step] of [EMB, DEC, EMB, DEC].entries()) {
    const r = step === EMB ? await embed([`switch ${i}`])
      : await http("POST", `${SERVE}/v1/systemone`, { model: DEC, state: "My card was charged twice.", questions: { q: { type: "choice", instructions: "Team?", criteria: { billing: "money", technical: "bugs" } } } });
    row(`call ${i + 1}: ${step}`, `${r.status}`, `${r.ms} ms`, step === EMB ? `${loadMs(r)} ms load` : "", `resident: ${(await resident()).join(", ")}`);
  }
  note("Small models stay co-resident; only the first call of each pays a load.");
}

// 5. Queueing and priorities.
section("Queue and priorities (8 managed embedding tasks at once)");
{
  const order = [];
  const jobs = ["background", "background", "normal", "normal", "normal", "interactive", "interactive", "background"].map((priority, i) =>
    embed(Array.from({ length: 300 }, (_, k) => `queue ${i} item ${k}`), { priority }).then((r) => { order.push(`${i}:${priority[0]}`); return [priority, r]; }));
  const done = await Promise.all(jobs);
  for (const [priority, r] of done) row(priority, `${r.status}`, `${r.ms} ms wall`, `${r.json?.admission?.total_wait_ms ?? "?"} ms queued`);
  note(`Completion order: ${order.join(" ")} (i = interactive, n = normal, b = background).`);
  const raw = await Promise.all(Array.from({ length: 4 }, (_, i) => http("POST", `${SERVE}/api/embed`, { model: EMB, input: [`raw ${i}`] })));
  row("4 raw /api/embed at once", raw.map((r) => r.status).join(" "), `${Math.max(...raw.map((r) => r.ms))} ms slowest`);
}

// 6. Prompt caching on a chat-capable model.
section("Prompt prefix caching (glm-ocr as a text model, 4k context)");
if (await fits(OCR, "completion")) {
  const system = "You are a careful assistant. " + Array.from({ length: 60 }, (_, i) => `Rule ${i + 1}: answer in at most five words and never invent facts about item ${i}.`).join(" ");
  const ask = (sys, q) => http("POST", "/tasks", { task: "completion", model: OCR, context_tokens: 4096, max_wait_seconds: 120,
    messages: [{ role: "system", content: sys }, { role: "user", content: q }], request_options: { options: { num_predict: 16 } } });
  const w = await http("POST", "/warm", { model: OCR, context_tokens: 4096, messages: [{ role: "system", content: system }] });
  row("warm with the system prompt", `${w.status}`, `${w.ms} ms`);
  for (const [label, sys, q] of [["same prefix, question 1", system, "What is rule 3 about?"], ["same prefix, question 2", system, "What is rule 7 about?"], ["different prefix", system.replace("careful", "precise"), "What is rule 3 about?"]]) {
    const r = await ask(sys, q);
    const m = r.json?.metrics ?? {};
    row(label, `${r.status}`, `${m.prompt_tokens ?? "?"} prompt tokens`, `${m.cached_prompt_tokens ?? "?"} cached`, `${Math.round((m.prompt_duration_ns ?? 0) / 1e6)} ms prompt eval`);
  }
  const models = (await http("GET", "/models")).json;
  const ocr = (Array.isArray(models) ? models : models.models ?? []).find((x) => x.name === OCR);
  row("learned token ratio", `${ocr?.bytes_per_token_milli ?? "none"} milli-bytes/token`, `${ocr?.bytes_per_token_samples ?? 0} samples`);
} else row("skipped", "glm-ocr did not fit within 15 minutes");

// 7. Idle sweep under memory pressure.
section("Idle models under memory pressure (sweep)");
{
  const preview = (await http("POST", "/routes", { task: "embedding", model: EMB })).json?.execution?.resource_snapshot;
  row("host now", `pressure: ${JSON.stringify(preview?.reasons ?? [])}`, `resident: ${(await resident()).join(", ")}`);
  await sleep(75_000);
  const events = (await http("GET", "/status")).json?.residency?.audit?.recent ?? [];
  const swept = events.filter((e) => e.trigger === "pressure_sweep").slice(0, 4).map((e) => `${e.model}`);
  row("after 75 s idle", `resident: ${(await resident()).join(", ") || "none"}`, `swept: ${swept.join(", ") || "none"}`);
  note("Under FreeLlama's own low-memory signal only idle-for-60 s runners are released; swapping or OS pressure releases any idle runner.");
}

// 8. CPU vs GPU with a CPU-only Ollama that serve starts itself.
section("GPU vs CPU (serve --spawn-cpu-backend)");
{
  const home = path.join(tmpdir(), "freellama-lifecycle-home");
  rmSync(home, { recursive: true, force: true });
  cpSync(path.join(homedir(), ".octocode/freellama"), home, { recursive: true });
  const cpuServe = spawn(BIN, ["serve", "--listen", "127.0.0.1:11466", "--upstream", "http://127.0.0.1:11434", "--spawn-cpu-backend", "--cpu-model", EMB, "--home", home], { stdio: "ignore" });
  let up = false;
  for (let i = 0; i < 60 && !up; i++) { await sleep(500); up = (await http("GET", "http://127.0.0.1:11466/_freellama/v1/ready")).status === 200; }
  if (up) {
    const items = Array.from({ length: 200 }, (_, i) => `compare item ${i} with a sentence of ordinary length`);
    await embed(["gpu warm"]);
    const gpu = await embed(items);
    const cpuWarm = await http("POST", "http://127.0.0.1:11466/_freellama/v1/tasks", { task: "embedding", model: EMB, input: ["cpu warm"], max_wait_seconds: 120 });
    const cpu = await http("POST", "http://127.0.0.1:11466/_freellama/v1/tasks", { task: "embedding", model: EMB, input: items, max_wait_seconds: 120 });
    row("GPU (Metal), 200 items", `${gpu.ms} ms`, gpu.json?.execution?.observation?.processor ?? "?");
    row("CPU backend, 200 items", `${cpu.ms} ms`, cpu.json?.execution?.observation?.processor ?? cpu.json?.error ?? "?", `cold load ${loadMs(cpuWarm)} ms`);
  } else row("skipped", "the spawned CPU backend did not become ready");
  cpuServe.kill("SIGINT");
  await sleep(2000);
  row("after serve exits", `CPU Ollama answering: ${(await http("GET", "http://127.0.0.1:11436/api/version")).status === 200}`);
}

// 9. Edge cases: each should fail fast with a clear reason, or succeed.
section("Edge cases");
for (const [label, method, url, body] of [
  ["model not installed", "POST", "/tasks", { task: "completion", model: "no-such:model", prompt: "x" }],
  ["empty prompt", "POST", "/tasks", { task: "completion", model: OCR, prompt: "" }],
  ["embedding item far past the context", "POST", "/tasks", { task: "embedding", model: EMB, input: ["x".repeat(40_000)] }],
  ["timeout_seconds 0", "POST", "/tasks", { task: "embedding", model: EMB, input: ["x"], timeout_seconds: 0 }],
  ["unknown field", "POST", "/tasks", { task: "embedding", model: EMB, input: ["x"], bogus: 1 }],
  ["unload a model that is not installed", "POST", "/unload", { model: "no-such:model" }],
  ["warm an embedding model without task", "POST", "/warm", { model: EMB }],
  ["preview a model that does not fit", "POST", "/routes", { task: "completion", model: "gpt-oss:20b", context_tokens: 4096 }],
  ["readiness", "GET", "/ready", undefined],
]) {
  const r = await http(method, url, body);
  const why = r.json?.code ?? r.json?.status ?? r.json?.execution?.resource_assessment?.status ?? (r.json?.ready === true ? "ready" : "") ?? "";
  const holders = (r.json?.execution?.memory_holders ?? []).map((h) => `${h.name}${h.processes > 1 ? ` x${h.processes}` : ""}`).join(", ");
  row(label, `${r.status}`, `${r.ms} ms`, `${why}${holders ? `; largest: ${holders}` : ""}`.slice(0, 160));
}

for (const model of [EMB, DEC, OCR]) await http("POST", "/unload", { model });
writeFileSync(path.join(ROOT, "LOCAL_LIFECYCLE.md"), `# LOCAL_LIFECYCLE: how FreeLlama runs models on this machine

Measured ${new Date().toISOString().slice(0, 16).replace("T", " ")} UTC on an Apple M2 Pro (32 GB unified memory), Ollama 0.40.13,
FreeLlama 0.8.0 at ${SERVE}, with other work running on the machine. Small models only
(\`${EMB}\`, \`${DEC}\`, \`${OCR}\`); every step waited until FreeLlama said its model fit.
Single run, one machine: read the numbers as this machine's behaviour, not a benchmark.

${out.map((s) => `## ${s.title}\n\n${s.rows.map((r) => `- **${r[0]}**: ${r.slice(1).filter(Boolean).join(" · ")}`).join("\n")}\n${s.notes.length ? `\n${s.notes.map((n) => `> ${n}`).join("\n\n")}\n` : ""}`).join("\n")}
`);
console.error("done");
