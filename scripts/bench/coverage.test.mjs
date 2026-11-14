import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { CASE, runBenchmark, summarizeConcurrency, validateRunOptions } from './lib.mjs';
import { parseRunArgs } from './run-args.mjs';

const answer = JSON.stringify(CASE.expected);
const ndjson = () => new Response(JSON.stringify({ message: { content: answer } }) + '\n' + JSON.stringify({ done: true, eval_count: 20, eval_duration: 1e9, prompt_eval_count: 80, load_duration: 250e6 }) + '\n');
const sseOpenAI = () => new Response(`data: ${JSON.stringify({ choices: [{ delta: { content: answer } }] })}\n\ndata: [DONE]\n\n`);
const ollamaVendor = (id = 'a') => ({ id, protocol: 'ollama', base_url: `http://${id}.invalid`, model: 'fixture:1b' });
const openaiVendor = (id = 'b', extra = {}) => ({ id, protocol: 'openai', base_url: `http://${id}.invalid`, model: 'bench-model', ...extra });
const noProcesses = async () => null;
const lines = async file => (await readFile(file, 'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse);

// One fake host: vendor endpoints plus an Ollama daemon whose residency follows inference.
function fakeHost({ residentAtStart = false, delayMs = 0 } = {}) {
  const state = { resident: residentAtStart, unloads: 0, active: 0, maxActive: 0, inference: [] };
  state.fetchImpl = async (url, options = {}) => {
    const { hostname, pathname } = new URL(url);
    if (hostname === 'ollama.invalid') {
      if (pathname === '/api/ps') return Response.json({ models: state.resident ? [{ name: 'fixture:1b', size: 3e9, size_vram: 2.5e9 }] : [] });
      if (pathname === '/api/generate') { assert.deepEqual(JSON.parse(options.body), { model: 'fixture:1b', keep_alive: 0 }); state.unloads++; state.resident = false; return Response.json({ done: true }); }
      throw new Error(`Unexpected Ollama route ${pathname}`);
    }
    if (pathname === '/') throw new Error('No listener before start');
    if (pathname === '/api/version') return Response.json({ version: 'fixture' });
    if (pathname === '/api/tags') return Response.json({ models: [{ name: 'fixture:1b' }] });
    if (pathname === '/v1/models') return Response.json({ data: [{ id: 'bench-model' }] });
    state.inference.push(`${hostname}${pathname}`);
    state.active++; state.maxActive = Math.max(state.maxActive, state.active);
    try {
      if (delayMs) await new Promise(resolve => setTimeout(resolve, delayMs));
      if (pathname === '/api/chat') { state.resident = true; return ndjson(); }
      return sseOpenAI();
    } finally { state.active--; }
  };
  return state;
}
async function withDir(run) {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-coverage-'));
  try { return await run(directory); } finally { await rm(directory, { recursive: true, force: true }); }
}
const base = (directory, extra) => ({ out: path.join(directory, 'out'), samples: 2, warmups: 1, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: () => {}, processSampler: noProcesses, ...extra });

test('run arguments: defaults stay sequential and dependent flags are enforced', () => {
  const plain = parseRunArgs(['--config', 'c.json']);
  assert.deepEqual([plain.samples, plain.warmups, plain.concurrency, plain.cold, plain.keepLoaded, plain.hogGb], [30, 2, [], false, false, undefined]);
  const full = parseRunArgs(['--config', 'c.json', '--keep-loaded', '--max-load-per-cpu', '0.75', '--strict-load', '--concurrency', '2, 4,8', '--cold', '--cold-samples', '5', '--hog-gb', '1.5', '--force-hog']);
  assert.deepEqual([full.keepLoaded, full.maxLoadPerCpu, full.strictLoad, full.concurrency, full.cold, full.coldSamples, full.hogGb, full.forceHog], [true, 0.75, true, [2, 4, 8], true, 5, 1.5, true]);
  for (const [args, message] of [[['--config', 'c', '--strict-load'], /requires --max-load-per-cpu/], [['--config', 'c', '--hog-gb', '1'], /requires --cold/], [['--config', 'c', '--cold-samples', '2'], /requires --cold/], [['--config', 'c', '--cold', '--force-hog'], /requires --hog-gb/], [['--config', 'c', '--concurrency', '2,x'], /integer/], [['--config', 'c', '--max-load-per-cpu', '-1'], /positive/], [['--bogus'], /Unknown argument/], [[], /--config is required/]]) assert.throws(() => parseRunArgs(args), message);
  assert.throws(() => validateRunOptions({ concurrency: [2, 2] }), /unique/);
  assert.throws(() => validateRunOptions({ concurrency: [65] }), /1 to 64/);
  assert.throws(() => validateRunOptions({ hogGb: 1 }), /cold/);
  assert.doesNotThrow(() => validateRunOptions({ concurrency: [1, 2], cold: true, hogGb: 0.5 }));
});

test('strict load gate records the block as not run without inference; warn mode still runs', async () => withDir(async directory => {
  const host = fakeHost();
  const loadProbe = async () => ({ load1: 12, load1_per_cpu: 3, logical_cpus: 4, top_processes: [{ pcpu: 300, command: 'busy' }] });
  const report = await runBenchmark({ ...base(directory), config: { model: {}, vendors: [ollamaVendor('a'), openaiVendor('b')] }, fetchImpl: host.fetchImpl, maxLoadPerCpu: 1, strictLoad: true, loadProbe });
  assert.deepEqual(report.vendors.map(row => row.status), ['not_run', 'not_run']);
  assert.equal(host.inference.length, 0);
  assert.match(report.vendors[0].error, /Load gate: 1-minute load 3\.00 per CPU exceeds 1\. Top: busy 300%/);
  assert.equal(report.vendors[0].load_gate.action, 'refuse');
  assert.equal(JSON.parse(await readFile(path.join(directory, 'out/a/load-gate.json'), 'utf8')).top_processes[0].command, 'busy');
  assert.match(await readFile(path.join(directory, 'out/SUMMARY.md'), 'utf8'), /## Load at block start/);
  const warnings = [];
  const warned = await runBenchmark({ ...base(directory), out: path.join(directory, 'warn'), config: { model: {}, vendors: [openaiVendor('b')] }, fetchImpl: host.fetchImpl, maxLoadPerCpu: 1, loadProbe, onProgress: message => warnings.push(message) });
  assert.equal(warned.vendors[0].status, 'complete');
  assert.equal(warned.vendors[0].load_gate.action, 'warn');
  assert.ok(warnings.some(message => /^Warning: b starts at load 3\.00 per CPU/.test(message)));
}));

test('after an Ollama-backed block the harness unloads the model it warmed and records size_vram', async () => withDir(async directory => {
  const host = fakeHost();
  const config = { ollama_base_url: 'http://ollama.invalid', model: { ollama_tag: 'fixture:1b' }, vendors: [ollamaVendor('a'), openaiVendor('b')] };
  const report = await runBenchmark({ ...base(directory), config, fetchImpl: host.fetchImpl, observeOllama: true, unloadAfterBlock: true });
  const [a, b] = report.vendors;
  assert.equal(a.ollama_residency_before.resident, false);
  assert.equal(a.ollama_residency_end.resident, true);
  assert.equal(a.ollama_residency_end.size_vram_bytes, 2.5e9);
  assert.equal(a.unload.confirmed_absent, true);
  assert.equal(host.unloads, 1);
  assert.equal(host.resident, false);
  assert.equal(Object.hasOwn(b, 'unload'), false);
  assert.equal(report.coverage.ollama.base_url, 'http://ollama.invalid');
  const summary = await readFile(path.join(directory, 'out/SUMMARY.md'), 'utf8');
  assert.match(summary, /- a: unloaded, confirmed by \/api\/ps/);
  assert.match(summary, /Ollama size_vram at end \(MiB\)/);
  assert.match(await readFile(path.join(directory, 'out/summary.csv'), 'utf8'), /unload_outcome/);
}));

test('a model resident before the block, an uncertain block, or --keep-loaded is never unloaded', async () => withDir(async directory => {
  const config = { ollama_base_url: 'http://ollama.invalid', model: { ollama_tag: 'fixture:1b' }, vendors: [ollamaVendor('a')] };
  const preexisting = fakeHost({ residentAtStart: true });
  let report = await runBenchmark({ ...base(directory), config, fetchImpl: preexisting.fetchImpl, observeOllama: true, unloadAfterBlock: true });
  assert.deepEqual(report.vendors[0].unload, { model: 'fixture:1b', requested: false, skipped_reason: 'resident_before_block_not_warmed_by_this_run' });
  assert.equal(preexisting.unloads, 0);
  const kept = fakeHost();
  report = await runBenchmark({ ...base(directory), out: path.join(directory, 'kept'), config, fetchImpl: kept.fetchImpl, observeOllama: true, unloadAfterBlock: false });
  assert.equal(kept.unloads, 0);
  assert.equal(report.vendors[0].ollama_residency_end.size_vram_bytes, 2.5e9);
  const uncertain = fakeHost();
  const fetchImpl = async (url, options) => new URL(url).pathname === '/api/chat' ? (() => { throw new Error('Connection lost after submission'); })() : uncertain.fetchImpl(url, options);
  report = await runBenchmark({ ...base(directory), out: path.join(directory, 'uncertain'), warmups: 0, config, fetchImpl, observeOllama: true, unloadAfterBlock: true });
  assert.equal(report.vendors[0].unload.skipped_reason, 'execution_uncertain');
  assert.equal(uncertain.unloads, 0);
}));

test('concurrency levels run N clients on the warm model; default output has no concurrency data', async () => withDir(async directory => {
  const host = fakeHost({ delayMs: 15 });
  const config = { model: {}, vendors: [openaiVendor('b')] };
  const plain = await runBenchmark({ ...base(directory), config, fetchImpl: host.fetchImpl });
  assert.equal(host.maxActive, 1);
  assert.equal(Object.hasOwn(plain.vendors[0], 'concurrency'), false);
  assert.equal(Object.hasOwn(plain, 'coverage'), false);
  await assert.rejects(stat(path.join(directory, 'out/concurrency.csv')), { code: 'ENOENT' });
  const levels = [];
  const report = await runBenchmark({ ...base(directory), out: path.join(directory, 'conc'), samples: 4, config, concurrency: [1, 2, 4], fetchImpl: async (url, options) => {
    const response = await host.fetchImpl(url, options);
    if (new URL(url).pathname === '/v1/chat/completions') levels.push(host.maxActive);
    return response;
  } });
  const [two, four] = report.vendors[0].concurrency;
  assert.deepEqual(report.coverage.concurrency_levels, [2, 4]);
  assert.deepEqual([two.level, two.attempted, two.success, two.failures], [2, 4, 4, 0]);
  assert.deepEqual([four.level, four.attempted, four.success], [4, 4, 4]);
  assert.equal(host.maxActive, 4);
  assert.ok(four.wall_window_ms > 0 && four.wall_verified_per_minute > 0);
  assert.equal(four.verified_elapsed_ms.n, 4);
  assert.equal(report.vendors[0].attempted, 4);
  assert.equal((await lines(path.join(directory, 'conc/b/concurrency-2.jsonl'))).length, 4);
  assert.ok((await lines(path.join(directory, 'conc/b/concurrency-4.jsonl'))).every(trial => trial.concurrency === 4 && trial.warmup === false));
  assert.match(await readFile(path.join(directory, 'conc/SUMMARY.md'), 'utf8'), /## Concurrency \(C1\)/);
  assert.match(await readFile(path.join(directory, 'conc/concurrency.csv'), 'utf8'), /^"vendor","clients"/);
}));

test('concurrency summary counts failures and keeps unknown wall windows unknown', () => {
  const trial = (outcome, start, end) => ({ outcome, client_start_ms: start, client_end_ms: end, elapsed_ms: end - start, first_content_ms: 5 });
  const level = summarizeConcurrency(2, [trial('verified', 0, 100), trial('failed', 10, 60), trial('verified', 50, 200)]);
  assert.deepEqual([level.attempted, level.success, level.failed, level.failures, level.wall_window_ms], [3, 2, 1, 1, 200]);
  assert.equal(level.wall_verified_per_minute, 600);
  assert.equal(summarizeConcurrency(2, [{ ...trial('verified', 0, 1), client_end_ms: null }]).wall_window_ms, null);
});

test('cold mode unloads Ollama-backed models before each cold sample and records vendor load time', async () => withDir(async directory => {
  const host = fakeHost();
  const config = { ollama_base_url: 'http://ollama.invalid', model: { ollama_tag: 'fixture:1b' }, vendors: [ollamaVendor('a'), openaiVendor('b')] };
  const report = await runBenchmark({ ...base(directory), config, fetchImpl: host.fetchImpl, cold: true, coldSamples: 2 });
  const [a, b] = report.vendors;
  assert.equal(host.unloads, 2);
  assert.deepEqual([a.cold.method, a.cold.status, a.cold.attempted, a.cold.success, a.cold.unload_confirmed], ['ollama_keep_alive_0', 'complete', 2, 2, 2]);
  assert.equal(a.cold.load_ms.p50, 250);
  assert.equal(a.attempted, 2);
  const coldTrials = await lines(path.join(directory, 'out/a/cold.jsonl'));
  assert.ok(coldTrials.every(trial => trial.cold === true && trial.cold_preparation.unload.confirmed_absent));
  assert.equal(b.cold.status, 'unsupported');
  assert.match(b.cold.reason, /cannot unload/);
  assert.match(await readFile(path.join(directory, 'out/SUMMARY.md'), 'utf8'), /## Cold load \(C3\)/);
}));

test('cold mode restarts owned processes with fresh logs and tracks the owned group RSS', async () => withDir(async directory => {
  const host = fakeHost();
  const starts = [], stops = [], sampled = [];
  let pid = 500;
  const startProcessImpl = async (vendor, dir, { logName }) => { starts.push(logName); const child = { pid: ++pid }; return { child, assertRunning() {}, async stop() { stops.push(child.pid); } }; };
  const processSampler = async ({ ownedPgid, includeOllamaRunners }) => { sampled.push([ownedPgid, includeOllamaRunners]); return { owned_rss_bytes: ownedPgid * 1024, tracked_rss_bytes: ownedPgid * 1024 }; };
  const report = await runBenchmark({ ...base(directory), config: { model: {}, vendors: [openaiVendor('b', { start: { command: 'fixture', args: [] } })] }, fetchImpl: host.fetchImpl, startProcessImpl, processSampler, cold: true, coldSamples: 2 });
  const row = report.vendors[0];
  assert.deepEqual(starts, ['server.log', 'server-cold-1.log', 'server-cold-2.log']);
  assert.deepEqual(stops, [501, 502, 503]);
  assert.equal(row.cold.method, 'owned_restart');
  assert.equal(row.cold.success, 2);
  assert.equal(row.cold.restart_to_ready_ms.n, 2);
  assert.equal(row.status, 'complete');
  assert.ok(sampled.length >= 1 && sampled.every(([group, runners]) => group > 500 && runners === false));
  assert.ok(row.host_observations.process_rss.peak_tracked_bytes >= 501 * 1024);
  assert.match(await readFile(path.join(directory, 'out/SUMMARY.md'), 'utf8'), /Peak RSS \(MiB\)/);
}));

test('memory hog: refused above the limit as not run; otherwise held for the block and stopped', async () => withDir(async directory => {
  const host = fakeHost();
  const config = { model: {}, vendors: [openaiVendor('b')] };
  let started = 0, stopped = 0;
  const startHogImpl = async ({ gb, force }) => { started++; assert.equal(gb, 0.01); assert.equal(force, false); return { pid: 1, ready: { ready: true, bytes: 10 }, async stop() { stopped++; return { code: 0 }; } }; };
  const tight = await runBenchmark({ ...base(directory), config, fetchImpl: host.fetchImpl, cold: true, coldSamples: 1, hogGb: 4, hostProbe: async () => ({ available_bytes: 5 * 1024 ** 3, swap_used_bytes: 0 }), startHogImpl });
  assert.equal(tight.vendors[0].status, 'not_run');
  assert.match(tight.vendors[0].error, /Memory hog refused.*--force-hog/);
  assert.equal(started, 0);
  assert.equal(host.inference.length, 0);
  let swap = 100;
  const report = await runBenchmark({ ...base(directory), out: path.join(directory, 'hog'), config, fetchImpl: host.fetchImpl, cold: true, coldSamples: 1, hogGb: 0.01, hostProbe: async () => ({ available_bytes: 8 * 1024 ** 3, available_bytes_source: 'freellama_estimate', swap_used_bytes: swap += 1024 ** 2 }), startHogImpl });
  const hog = report.vendors[0].memory_hog;
  assert.deepEqual([started, stopped, hog.allowed, hog.ready.bytes, hog.stop.code], [1, 1, true, 10, 0]);
  assert.ok(hog.swap_after_ready_bytes > hog.swap_before_bytes && hog.swap_after_stop_bytes > hog.swap_after_ready_bytes);
  assert.equal(report.coverage.memory_hog_gb, 0.01);
  assert.match(await readFile(path.join(directory, 'hog/SUMMARY.md'), 'utf8'), /## Memory hog \(C2\)/);
}));
