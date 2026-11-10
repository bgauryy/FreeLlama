#!/usr/bin/env node
// Per-workflow quality eval: each text workflow's cases on each candidate model, run exactly as an
// agent runs them (`fllama run -w WORKFLOW -m MODEL`, so the same prompts, options and checks),
// graded deterministically: required facts present, invented ones absent, length bounds, filler,
// exact JSON fields, the planted bug named. Needs a running Ollama and the built fllama
// (`yarn build`); models must already be installed (nothing is pulled).
//
//   node scripts/local-test/workflows.mjs [--models a,b] [--workflows x,y] [--out report.json]
//
// The report gives pass rates per workflow and model; a workflow whose default pick fails where a
// stronger installed pick passes is a candidate for `prefer: "capable"` in local-prompts.ts.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const CLI = path.join(root, "packages/mcp/dist/agent-cli.js");

/** Phrases that pad an answer without adding to it. */
const FILLER = [/^(sure|certainly|of course)[,!.]/im, /here (is|are) (the|a|your) /i, /as an ai\b/i, /only one sentence/i, /i hope this helps/i, /let me know if/i, /in summary,? (the|this) (text|input|log)/i];

const words = (text) => text.split(/\s+/).filter(Boolean).length;
const has = (text, term) => (term instanceof RegExp ? term.test(text) : text.toLowerCase().includes(term.toLowerCase()));

/**
 * One case: `input` (piped as stdin), optional `prompt`, and graders. `all`: every term must
 * appear; `any`: at least one per group; `none`: none may appear; `maxWords`; `json`: exact fields.
 */
const CASES = {
  "summarize-text": [
    { id: "ST1", input: "The deploy on Tuesday failed because the disk on node 3 was full. We freed 40 GB and redeployed at 14:10; the service has been healthy since.", maxWords: 45,
      any: [["disk", "storage"], ["node 3"], ["redeploy", "redeployed", "healthy"]], none: ["database", "network outage"] },
    { id: "ST2", input: "Q3 revenue was 1.24M, up 22% from Q2. Churn fell from 4.1% to 3.2%. Hiring is frozen until January. The mobile app launch moved from October to December.", maxWords: 60,
      any: [["1.24", "22%"], ["churn"], ["hiring", "freeze", "frozen"], ["december", "mobile"]], none: ["layoff"] },
    { id: "ST3", input: "Please note that the office will be closed on Friday for maintenance.", maxWords: 25, any: [["closed", "close"], ["friday"]], none: ["monday", "holiday"] },
  ],
  "summarize-log": [
    { id: "SL1", input: ["2026-11-10T09:00:01Z INFO start", "2026-11-10T09:00:05Z ERROR db: connection refused (port 5432)", "2026-11-10T09:00:06Z ERROR db: connection refused (port 5432)", "2026-11-10T09:00:07Z WARN retrying in 5s", "2026-11-10T09:00:12Z ERROR db: connection refused (port 5432)", "2026-11-10T09:00:13Z INFO shutting down"].join("\n"), maxWords: 90,
      any: [["connection refused"], ["5432"], ["3", "three", "×3", "x3"]], none: ["timeout", "out of memory"] },
    { id: "SL2", input: ["12:00:00 GET /api/users 200 12ms", "12:00:01 GET /api/orders 500 3001ms upstream timeout", "12:00:02 GET /api/users 200 10ms", "12:00:03 GET /api/orders 500 3002ms upstream timeout", "12:00:04 POST /api/login 401 5ms"].join("\n"), maxWords: 90,
      any: [["/api/orders"], ["500"], ["timeout"]], none: ["database"] },
  ],
  "summarize-diff": [
    { id: "SD1", input: ["--- a/pager.py", "+++ b/pager.py", "@@ -1,4 +1,4 @@", " def page(items, n):", "-    return [items[i:i+n] for i in range(0, len(items), n)]", "+    if n <= 0:", "+        raise ValueError(\"n must be positive\")", "+    return [items[i:i+n] for i in range(0, len(items), n)]"].join("\n"), maxWords: 90,
      any: [["valueerror", "raise", "error"], ["n <= 0", "positive", "non-positive", "zero"]], none: ["removes", "deleted the function"] },
  ],
  "extract-json": [
    { id: "EJ1", input: "Invoice 2291 from Acme Corp, issued 2026-10-02, total 1,240.50 EUR, due in 30 days.", prompt: "invoice_id, vendor, total, currency",
      json: { invoice_id: (v) => String(v) === "2291", vendor: (v) => /acme/i.test(String(v)), total: (v) => Number(String(v).replace(/,/g, "")) === 1240.5, currency: (v) => /eur/i.test(String(v)) } },
    { id: "EJ2", input: "Ticket from dana@example.org: the export button crashes the app on Android 15 since version 4.2.", prompt: "email, platform, version",
      json: { email: (v) => v === "dana@example.org", platform: (v) => /android/i.test(String(v)), version: (v) => String(v).includes("4.2") } },
  ],
  "explain-code": [
    { id: "EC1", input: "def retry(fn, attempts=3, delay=1.0):\n    for i in range(attempts):\n        try:\n            return fn()\n        except Exception:\n            if i == attempts - 1:\n                raise\n            time.sleep(delay * 2 ** i)\n", maxWords: 220,
      any: [["retry", "retries"], ["exponential", "doubl", "2 **", "backoff"], ["raise", "re-raise", "last attempt"]] },
  ],
  "review-code": [
    { id: "RC1", input: "def average(xs):\n    return sum(xs) / len(xs)\n", any: [["empty", "zero", "zerodivision", "len(xs) == 0", "division by zero"]] },
    { id: "RC2", input: "def last_n(items, n):\n    return items[len(items) - n - 1:]\n", any: [["off-by-one", "off by one", "one extra", "n + 1", "n+1", "- 1"]] },
  ],
  "fix-bug": [
    { id: "FB1", input: "def average(xs):\n    return sum(xs) / len(xs)\n", prompt: "average([]) raises ZeroDivisionError; return 0.0 for an empty list.", any: [["if not xs", "len(xs) == 0", "if xs", "== 0"], ["0.0", "0"]] },
  ],
};

function argList(flag) {
  const at = process.argv.indexOf(flag);
  return at > 0 ? process.argv[at + 1]?.split(",").filter(Boolean) : undefined;
}

/** The models to compare: --models, else every installed catalog pick for coding (the text workflows' job picks). */
function candidateModels() {
  const given = argList("--models");
  if (given) return given;
  const text = execFileSync("node", [CLI, "catalog", "coding"], { encoding: "utf8" });
  return [...text.matchAll(/([\w./:-]+) \((?:default|stronger|alternative|small)[^)]*installed, fits now\)/g)].map((match) => match[1]);
}

function runCase(workflow, model, item) {
  const args = [CLI, "run", "-w", workflow, "-m", model, "--json", "-o", "temperature=0", ...(item.prompt ? [item.prompt] : [])];
  const started = Date.now();
  let receipt;
  try {
    receipt = JSON.parse(execFileSync("node", args, { input: item.input, encoding: "utf8", timeout: 300_000, maxBuffer: 64 * 1024 * 1024 }));
  } catch (error) {
    return { id: item.id, pass: false, failures: [`run failed: ${String(error.stdout || error.message).slice(0, 200)}`], seconds: (Date.now() - started) / 1000 };
  }
  const answer = String(receipt?.response?.message?.content ?? "").trim();
  const failures = [];
  if (answer === "") failures.push("empty answer");
  for (const term of item.all ?? []) if (!has(answer, term)) failures.push(`missing "${term}"`);
  for (const group of item.any ?? []) if (!group.some((term) => has(answer, term))) failures.push(`missing one of ${JSON.stringify(group)}`);
  for (const term of item.none ?? []) if (has(answer, term)) failures.push(`invented "${term}"`);
  if (item.maxWords && words(answer) > item.maxWords) failures.push(`${words(answer)} words > ${item.maxWords}`);
  const filler = FILLER.filter((pattern) => pattern.test(answer));
  if (filler.length > 0) failures.push(`filler: ${filler.map(String).join(", ")}`);
  if (item.json) {
    let parsed;
    try {
      parsed = JSON.parse(answer.replace(/^```(?:json)?\s*|\s*```$/g, ""));
    } catch {
      failures.push("not JSON");
    }
    for (const [field, ok] of Object.entries(item.json)) if (parsed && !ok(parsed[field])) failures.push(`field ${field} = ${JSON.stringify(parsed?.[field])}`);
  }
  return { id: item.id, pass: failures.length === 0, failures, words: words(answer), outputTokens: receipt?.metrics?.output_tokens, seconds: (Date.now() - started) / 1000 };
}

const workflows = argList("--workflows") ?? Object.keys(CASES);
const models = candidateModels();
if (models.length === 0) {
  console.error("no candidate model is installed and fits now; pass --models");
  process.exit(1);
}
const report = { date: new Date().toISOString(), models, results: {} };
for (const workflow of workflows) {
  report.results[workflow] = {};
  for (const model of models) {
    const rows = CASES[workflow].map((item) => runCase(workflow, model, item));
    const passed = rows.filter((row) => row.pass).length;
    report.results[workflow][model] = { passed, total: rows.length, rows };
    const median = (values) => values.sort((a, b) => a - b)[Math.floor(values.length / 2)];
    console.log(`${workflow.padEnd(15)} ${model.padEnd(48)} ${passed}/${rows.length}  median ${median(rows.map((row) => row.seconds)).toFixed(1)} s${rows.filter((row) => !row.pass).map((row) => `\n    ${row.id}: ${row.failures.join("; ")}`).join("")}`);
  }
}
const out = argList("--out")?.[0];
if (out) writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
