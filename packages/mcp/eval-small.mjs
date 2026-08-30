// Deep-research eval: does delegate_research still work as the local model shrinks?
// Every ground truth below was verified by grep against this repo before the run.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { writeFileSync } from "node:fs";

const WS = "/Users/guybary/Documents/code/FreeLlama";
const QUESTIONS = [
  ["Q1", "In Cargo.toml, what feature name does the [[bin]] required-features field list?", /\bcli\b/i],
  ["Q2", "In packages/rust-core/src/napi.rs, what integer value is the constant DEFAULT_TASK_TIMEOUT_SECS set to?", /\b900\b/],
  ["Q3", "In packages/rust-core/src/lib.rs, what is the exact name of the public function that builds the OLLAMA_MAX_LOADED_MODELS advisory?", /max_loaded_models_advisory/],
  ["Q4", "In packages/rust-core/src/platform.rs, the function route_evidence matches on (policy_qualified, has_benchmark). What confidence string does it return for (true, true)?", /\bmedium\b/i],
  ["Q5", "What is the exact value of the \"name\" field in packages/mcp/package.json?", /freellama-packages/mcp/],
  ["Q6", "In packages/rust-core/src/platform.rs, what is the string value of the API_ROOT constant?", /_freellama\/v1/],
  ["Q7", "In packages/cli/src/main.rs, what is the default bind address for the serve subcommand's --listen argument?", /11435/],
  ["Q8", "What version is declared in the [package] section of the root Cargo.toml?", /0\.2\.0/],
];
const MODELS = ["qwen2.5:0.5b", "llama3.2:3b", "qwen2.5:7b", "gemma4:12b-mlx", "qwen3.8:27b-mlx"];

const t = new StdioClientTransport({ command: "node", args: ["dist/index.js"] });
const c = new Client({ name: "eval", version: "0.0.1" }); await c.connect(t);
const rows = [];

for (const model of MODELS) {
  console.log(`\n${"=".repeat(72)}\n${model}\n${"=".repeat(72)}`);
  for (const [id, question, check] of QUESTIONS) {
    const started = Date.now();
    let rec = { id, model, ok: false, sec: 0, inTok: null, outTok: null, calls: 0, verdict: "n/a", err: null };
    try {
      const r = await c.callTool(
        { name: "delegate_research", arguments: { question, workspacePath: WS, model } },
        undefined, { timeout: 180000 },
      );
      rec.sec = +((Date.now() - started) / 1000).toFixed(1);
      if (r.isError) { rec.err = r.content[0].text.slice(0, 80); }
      else {
        const sc = r.structuredContent;
        rec.ok = check.test(sc.answer ?? "");
        rec.inTok = sc.usage?.inputTokens; rec.outTok = sc.usage?.outputTokens;
        rec.calls = sc.toolCallCount; rec.verdict = sc.verification?.recommendation;
      }
    } catch (e) {
      rec.sec = +((Date.now() - started) / 1000).toFixed(1);
      rec.err = e.message.slice(0, 80);
    }
    rows.push(rec);
    const mark = rec.err ? "ERR " : rec.ok ? "PASS" : "FAIL";
    console.log(`  ${id} ${mark} ${String(rec.sec).padStart(6)}s calls=${rec.calls} verdict=${rec.verdict}${rec.err ? " :: " + rec.err : ""}`);
  }
  // Unload before the next model. Co-residency of two large models has crashed this machine
  // before (skills/ollama-ops/references/model-selection.md), and the eval must not be the cause.
  await c.callTool({ name: "ollama_stop", arguments: { model } }).catch(() => {});
}

writeFileSync("eval-small-results.json", JSON.stringify(rows, null, 2));
console.log(`\n\n${"#".repeat(72)}\nSUMMARY\n${"#".repeat(72)}`);
console.log("model              pass  acc     med_s   med_in_tok  ungrounded  errors");
for (const model of MODELS) {
  const r = rows.filter((x) => x.model === model);
  const ok = r.filter((x) => x.ok).length;
  const med = (a) => { const v = a.filter((x) => x != null).sort((p, q) => p - q); return v.length ? v[Math.floor(v.length / 2)] : null; };
  const esc = r.filter((x) => x.verdict === "escalate").length;
  const errs = r.filter((x) => x.err).length;
  console.log(`${model.padEnd(18)} ${String(ok + "/" + r.length).padStart(5)} ${String(Math.round(100 * ok / r.length) + "%").padStart(5)} ${String(med(r.map((x) => x.sec))).padStart(7)} ${String(med(r.map((x) => x.inTok))).padStart(11)} ${String(esc).padStart(11)} ${String(errs).padStart(7)}`);
}
// Does the verification verdict I added actually predict correctness?
console.log("\nverdict vs correctness (all models pooled):");
for (const v of ["accept", "verify", "escalate"]) {
  const r = rows.filter((x) => x.verdict === v);
  if (r.length) console.log(`  ${v.padEnd(9)} n=${String(r.length).padStart(3)}  correct=${Math.round(100 * r.filter((x) => x.ok).length / r.length)}%`);
}
await c.close(); process.exit(0);
