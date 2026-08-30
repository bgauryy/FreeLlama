// Every tool exercised against the live system. Not a schema check — a behaviour check.
import assert from "node:assert/strict";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
const REPO = new URL("../..", import.meta.url).pathname.replace(/\/$/, "");
const t = new StdioClientTransport({ command: "node", args: ["dist/index.js"] });
const c = new Client({ name: "validate", version: "1" }); await c.connect(t);
const call = (n, a = {}, ms = 180000) => c.callTool({ name: n, arguments: a }, undefined, { timeout: ms });
const pass = [], fail = [];
const check = (n, ok, d = "") => (ok ? pass : fail).push(n + (d ? ` :: ${d}` : ""));

const d = (await call("doctor")).structuredContent;
check("doctor: 9 env vars w/ effective defaults", Object.keys(d.ollama_env_config).length === 9);
check("doctor: absorbed machine profile", !!d.machine?.unified_memory_bytes);
check("doctor: warning says 3x GPU not unlimited", /3 x GPU count/.test(d.ollama_env_config_warning ?? ""));

for (const v of ["installed", "resident", "raw"]) {
  const r = await call("models", { view: v });
  check(`models[${v}]`, !r.isError && !!r.structuredContent);
}
const det = (await call("models", { view: "detail", model: "qwen3.8:27b-mlx" })).structuredContent;
check("models[detail]: withholds blobs", det.license === undefined && det.modelfile === undefined);
check("models[detail]: real max context", typeof det.max_context_length === "number");
check("models[detail]: errors without model", (await call("models", { view: "detail" })).isError === true);

const rt = (await call("route", { task: "completion", objective: "fastest" })).structuredContent;
check("route: grades low w/o policy", rt.confidence === "low");
check("route: minConfidence refuses", (await call("route", { task: "completion", objective: "fastest", minConfidence: "medium" })).isError === true);
const t0 = Date.now();
const blocked = await call("run_task", { task: "completion", objective: "fastest", prompt: "hi", minConfidence: "medium" });
check("run_task: refuses BEFORE generating", blocked.isError === true && Date.now() - t0 < 5000, `${Date.now() - t0}ms`);


const emb = (await call("run_task", { task: "embedding", model: "nomic-embed-text:latest", input: "x", keepAlive: "0" })).structuredContent;
check("run_task: withholds vectors by default", !!emb.response.embeddings_omitted && emb.response.embeddings === undefined);
const full = (await call("run_task", { task: "embedding", model: "nomic-embed-text:latest", input: "x", keepAlive: "0", returnEmbeddings: true })).structuredContent;
check("run_task: returnEmbeddings opt-in works", Array.isArray(full.response.embeddings));

const s1 = (await call("search_models", { capabilities: ["vision"], limit: 5 }, 60000)).structuredContent;
check("search_models: popular default", s1.order === "popular" && !/o=newest/.test(s1.query));
check("search_models: points at step 2", /model:/.test(s1.nextStep));
const s2 = (await call("search_models", { model: "qwen3-vl" }, 60000)).structuredContent;
check("search_models: recommends a fitting tag", !!s2.recommendation && s2.tags.find((x) => x.tag === s2.recommendation.tag)?.fitsInMemory === true);
check("search_models: excludes what cannot fit", s2.tags.some((x) => (x.sizeBytes ?? 0) > 100e9 && x.fitsInMemory === false));

const esc = (await call("delegate_research", { question: "unanswerable from files", workspacePath: REPO, model: "qwen2.5:0.5b" })).structuredContent;
check("delegate_research: pre-flight refuses unusable model", esc.verification.recommendation === "escalate" && esc.toolCallCount === 0);
const ok = (await call("delegate_research", { question: "In packages/rust-core/Cargo.toml, what optional feature enables the Node addon?", workspacePath: REPO }, 300000)).structuredContent;
check("delegate_research: grounded answer accepted", /napi/i.test(ok.answer) && ok.verification.recommendation === "accept", `adapter=${ok.adapter}`);
check("delegate_research: defaults to bash adapter", ok.adapter === "bash");
check("model evidence is loaded from disk, not compiled in", !esc.verification.measuredBaseRate.includes("undefined"));

console.log(`PASS ${pass.length}:`); pass.forEach((x) => console.log("  ✓ " + x));
if (fail.length) { console.log(`FAIL ${fail.length}:`); fail.forEach((x) => console.log("  ✗ " + x)); }
await c.close(); process.exit(fail.length ? 1 : 0);
