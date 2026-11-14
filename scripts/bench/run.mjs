#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import path from "node:path";
import { runBenchmark } from "./lib.mjs";
import { parseRunArgs } from "./run-args.mjs";

const help = `Usage: node scripts/bench/run.mjs --config FILE [--out DIR] [--vendor ID]
       [--samples 30] [--warmups 2] [--allow-smoke] [--keep-loaded]
       [--max-load-per-cpu X [--strict-load]] [--concurrency 2,4,8]
       [--cold [--cold-samples 3] [--hog-gb N [--force-hog]]]

Runs the fixed S1 warm generation extraction case. Vendors execute strictly one by
one; requests are sequential unless --concurrency adds levels. No models are pulled.
FreeLlama supports raw admission (/api/chat), managed completion (/_freellama/v1/tasks)
and managed streamed completion (freellama-stream, stream:true) protocols. Managed
completion reports client wall time and receipt fields; its TTFT is unavailable
because it does not stream. Optional start commands use argv without a shell; only
processes owned by this run are stopped.

Memory: samples RSS of the owned process group and Ollama runners, and records Ollama
/api/ps size_vram at block end for Ollama-backed vendors (ollama, freellama*). After
such a block the bench model is unloaded with keep_alive 0 at config ollama_base_url
(default http://127.0.0.1:11434) and confirmed by /api/ps, only if this run warmed it.
--keep-loaded skips the unload.

--max-load-per-cpu X  record 1-minute load per CPU and the top 5 CPU processes at each
                      block start; warn above X. --strict-load skips the block (not run).
--concurrency 2,4,8   after the sequential samples, send the sample count through N
                      concurrent clients per level; records p50/p95, failures, throughput.
--cold                after warm samples, take --cold-samples cold samples per block:
                      unload (Ollama-backed) or restart (owned process), then time one request.
--hog-gb N            with --cold: hold N GiB of touched memory during each block. Refused
                      above estimated available minus 2 GiB unless --force-hog.

Output: per-vendor raw trials.jsonl, health-before.json, summary.json,
summary.csv, SUMMARY.md, and config.redacted.json. Existing output is never
overwritten. Fewer than 30 samples requires --allow-smoke. p99 remains exploratory
below 100 verified samples. The runner never declares a comparative winner.
`;

async function main() {
  const options = parseRunArgs(process.argv.slice(2));
  if (options.help) { console.log(help); return; }
  const configText = await readFile(path.resolve(options.config), "utf8");
  const controller = new AbortController();
  const abort = (signal) => controller.abort(new Error(`Interrupted by ${signal}.`));
  const interrupt = () => abort("SIGINT");
  const terminate = () => abort("SIGTERM");
  process.once("SIGINT", interrupt); process.once("SIGTERM", terminate);
  try {
    const out = path.resolve(options.out ?? `.octocode/bench/${new Date().toISOString().replaceAll(":", "-")}`);
    const report = await runBenchmark({ config: JSON.parse(configText), configText, out, samples: options.samples, warmups: options.warmups, allowSmoke: options.allowSmoke, vendorId: options.vendor, signal: controller.signal,
      observeOllama: true, unloadAfterBlock: !options.keepLoaded, maxLoadPerCpu: options.maxLoadPerCpu, strictLoad: options.strictLoad, concurrency: options.concurrency, cold: options.cold, coldSamples: options.coldSamples, hogGb: options.hogGb, forceHog: options.forceHog });
    console.log(JSON.stringify({ out, publication_status: report.publication_status, vendors: report.vendors.map(({ id, status, success, attempted, refused, failed, quality_failed }) => ({ id, status, success, attempted, refused, failed, quality_failed })) }, null, 2));
    // A block skipped by a gate is recorded as not run, not failed.
    if (report.vendors.some((vendor) => !["complete", "not_run"].includes(vendor.status))) process.exitCode = 1;
  } finally { process.removeListener("SIGINT", interrupt); process.removeListener("SIGTERM", terminate); }
}

main().catch((error) => { console.error(`${error.message ?? error}\n\n${help}`); process.exitCode = 1; });
