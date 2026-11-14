#!/usr/bin/env node
// FreeLlama feature benchmark: the management features, measured end to end through the MCP server
// an agent uses, against a fresh `serve` with its own home (nothing in the user's FreeLlama home is
// touched). Each scenario prints numbers; nothing is pulled, and models already installed are used.
//
//   node scripts/bench/features.mjs --binary target/release/freellama --out FILE.json
//     [--port 11451] [--chat-model bonsai-2:9b] [--decide-model strands-decider:2b-nvfp4]
//     [--scenarios priority,cache,index,sweep,long]
//
// priority  8 background tasks queued, then 1 interactive: its wait through FreeLlama vs Ollama alone
// cache     two questions about one long document: the second reuses Ollama's prompt cache
// index     search a code tree by meaning twice: the second embeds only what changed (nothing)
// sweep     judge every chunk of a code tree; then again with `baseline`, reusing unchanged chunks
// long      a long input as a deferred job: time to a job id, progress messages, total time
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const args = process.argv.slice(2);
const option = (flag, fallback) => { const index = args.indexOf(flag); return index >= 0 ? args[index + 1] : fallback; };
const binary = path.resolve(option("--binary", path.join(root, "target/release/freellama")));
const out = option("--out", "features.json");
const port = option("--port", "11451");
const chatModel = option("--chat-model", "bonsai-2:9b");
const decideModel = option("--decide-model", "strands-decider:2b-nvfp4");
const scenarios = option("--scenarios", "priority,cache,index,sweep,long").split(",");
const OLLAMA = "http://127.0.0.1:11434";
const SERVE = `http://127.0.0.1:${port}`;
const home = mkdtempSync(path.join(tmpdir(), "freellama-features-"));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const median = (xs) => { const s = [...xs].sort((a, b) => a - b); return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2; };
const round = (value, digits = 0) => Math.round(value * 10 ** digits) / 10 ** digits;

const serve = spawn(binary, ["serve", "--listen", `127.0.0.1:${port}`, "--upstream", OLLAMA], { stdio: "ignore", env: { ...process.env, FREELLAMA_HOME: home } });
const results = { date: new Date().toISOString(), chat_model: chatModel, scenarios: {} };
let client;
try {
  for (let i = 0; i < 120; i++) { try { if ((await fetch(`${SERVE}/_freellama/v1/health`)).ok) break; } catch { /* starting */ } await sleep(250); }
  client = new Client({ name: "freellama-features", version: "1" });
  await client.connect(new StdioClientTransport({
    command: process.execPath,
    args: [path.join(root, "packages/mcp/dist/index.js")],
    cwd: root,
    env: { ...process.env, FREELLAMA_HOME: home, FREELLAMA_SERVE_ENDPOINT: SERVE, FREELLAMA_MCP_RECEIPT: "structured", FREELLAMA_FILE_ROOTS: root },
  }));
  /** One MCP tool call: elapsed ms, structured result, text and progress messages. */
  const tool = async (name, input, timeout = 600_000) => {
    const progress = [];
    const started = performance.now();
    const result = await client.callTool({ name, arguments: input }, undefined, {
      timeout, resetTimeoutOnProgress: true, onprogress: (event) => progress.push({ at: performance.now() - started, message: event.message }),
    });
    const text = (result.content ?? []).map((part) => part.text ?? "").join("\n");
    if (result.isError) throw new Error(`${name}: ${text.slice(0, 400)}`);
    return { ms: performance.now() - started, data: result.structuredContent ?? {}, text, progress };
  };
  const run = async (key, scenario) => {
    if (!scenarios.includes(key)) return;
    try {
      results.scenarios[key] = await scenario();
    } catch (error) {
      results.scenarios[key] = { error: String(error.message ?? error).slice(0, 500) };
    }
    console.log(key, JSON.stringify(results.scenarios[key]));
  };

  await run("priority", async () => {
    const body = JSON.parse(readFileSync(path.join(root, "scripts/bench/features-case.json"), "utf8"));
    const timed = async (url, payload) => {
      const started = performance.now();
      const response = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload) });
      await response.json();
      if (!response.ok) throw new Error(`HTTP ${response.status} from ${url}`);
      return performance.now() - started;
    };
    const ollamaBody = { model: body.model, messages: body.messages, stream: false, think: false, options: { ...body.request_options.options, num_ctx: body.context_tokens } };
    const ollama = (payload) => timed(`${OLLAMA}/api/chat`, payload);
    const managed = (payload) => timed(`${SERVE}/_freellama/v1/tasks`, payload);
    for (let i = 0; i < 3; i++) { await ollama(ollamaBody); await managed(body); }
    const roundTrip = async (send, background, interactive) => {
      const started = performance.now();
      const batch = Array.from({ length: 8 }, () => send(background));
      await sleep(100);
      const urgent = await send(interactive);
      await Promise.all(batch);
      return { interactive: urgent, batch: performance.now() - started };
    };
    const sides = { ollama: [], freellama: [] };
    for (let r = 0; r < 7; r++) {
      sides.ollama.push(await roundTrip(ollama, ollamaBody, ollamaBody));
      sides.freellama.push(await roundTrip(managed, { ...body, priority: "background" }, { ...body, priority: "interactive" }));
    }
    return Object.fromEntries(Object.entries(sides).map(([side, list]) => [side, {
      interactive_median_ms: round(median(list.map((x) => x.interactive))),
      batch_median_ms: round(median(list.map((x) => x.batch))),
      rounds: list.length,
    }]));
  });

  await run("cache", async () => {
    // One long document as the shared prefix; two different questions about it.
    const document = readFileSync(path.join(root, "docs/ARCHITECTURE.md"), "utf8").slice(0, 12_000);
    const ask = (question) => tool("run_task", {
      task: "completion", model: chatModel, contextTokens: 8192, think: false, output: "inline", receipt: "full",
      options: { num_predict: 48, temperature: 0 },
      messages: [{ role: "system", content: `Answer from this document only.\n\n${document}` }, { role: "user", content: question }],
    });
    // A model reload empties the cache: start from a cold runner so the first question pays for the prefix.
    await tool("ollama_manage", { action: "warm", model: chatModel, contextTokens: 8192 });
    const first = await ask("Which process owns model routing?");
    const second = await ask("What does the memory gate do when a load does not fit?");
    const metrics = (call) => call.data.metrics ?? call.data.receipt?.metrics ?? {};
    const side = (call) => ({
      ms: round(call.ms), prompt_tokens: metrics(call).prompt_tokens ?? null,
      prompt_eval_ms: metrics(call).prompt_duration_ns ? round(metrics(call).prompt_duration_ns / 1e6) : null,
      cached_tokens: metrics(call).cached_prompt_tokens ?? metrics(call).prompt_eval_cached_count ?? null,
    });
    await tool("ollama_manage", { action: "unload", model: chatModel });
    return { document_chars: document.length, first_question: side(first), second_question: side(second) };
  });

  await run("index", async () => {
    const search = () => tool("run_task", { job: "find-code", files: ["packages/mcp/src/**/*.ts"], nearest: { query: "where a deferred job reports its progress", top: 3 } });
    const first = await search();
    const second = await search();
    const top = (call) => (call.text.match(/[\w/.-]+\.ts/g) ?? []).slice(0, 3);
    return { first_ms: round(first.ms), second_ms: round(second.ms), speedup: round(first.ms / second.ms, 1), top_first: top(first), top_second: top(second) };
  });

  await run("sweep", async () => {
    const questions = { secret_in_log: { type: "noul", instructions: "Does this code write a secret, token, password or API key to a log or to output?" } };
    const files = ["packages/mcp/src/tools/*.ts"];
    const sweep = async (extra) => {
      const started = performance.now();
      const start = await tool("sweep", { action: "start", files, questions, model: decideModel, ...extra });
      const sweepId = start.data.sweep_id ?? start.data.sweepId ?? start.text.match(/[0-9a-f-]{36}/)?.[0];
      let status = start.data;
      // A sweep reports `phase`; `wait` follows it for up to 50 s per call.
      while (!["completed", "failed", "cancelled", "interrupted"].includes(String(status.phase ?? status.status))) {
        status = (await tool("sweep", { action: "wait", sweepId })).data;
      }
      return { sweepId, ms: performance.now() - started, status };
    };
    const first = await sweep({});
    const again = await sweep({ baseline: first.sweepId });
    const counts = (status) => status.counts ?? status.progress ?? {};
    return { files: files[0], first_ms: round(first.ms), first_counts: counts(first.status), baseline_ms: round(again.ms), baseline_counts: counts(again.status), speedup: round(first.ms / again.ms, 1) };
  });

  await run("long", async () => {
    // A long log as one deferred job: the agent gets a job id at once and polls; nothing blocks it.
    const submitted = await tool("run_task", { workflow: "summarize-log", files: ["CHANGELOG.md"], defer: true });
    const jobId = submitted.data.job_id ?? submitted.text.match(/[0-9a-f-]{36}/)?.[0];
    const started = performance.now();
    let job = submitted.data;
    let polls = 0;
    while (!["completed", "failed", "cancelled", "expired"].includes(String(job.status ?? job.job?.status))) {
      await sleep(1000);
      job = (await tool("task_jobs", { action: "get", jobId })).data;
      polls += 1;
    }
    return { submit_ms: round(submitted.ms), job_total_ms: round(submitted.ms + performance.now() - started), polls, status: job.status ?? job.job?.status };
  });
} finally {
  await client?.close().catch(() => undefined);
  serve.kill("SIGTERM");
  writeFileSync(out, `${JSON.stringify(results, null, 2)}\n`);
  console.log(`wrote ${out}`);
}
