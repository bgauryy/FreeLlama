import { test } from 'node:test';
import assert from 'node:assert/strict';
import { evaluateLoadGate, isOllamaRunner, loadSnapshot, ollamaResidency, parseProcessTable, parseTopCpu, sampleProcessRss, summarizeProcessRss, summarizeRss, unloadOllamaModel } from './resources.mjs';
import { macAvailableEstimate, parseMacMemory, summarizeHost } from './host.mjs';

const table = [
  '  100     1   100   2048 /usr/local/bin/llama-server --port 18080',
  '  101   100   100   1024 /usr/local/bin/llama-server helper',
  '  200     1   200   4096 /Applications/Ollama.app/Contents/Resources/ollama serve',
  '  201   200   200 409600 /Applications/Ollama.app/Contents/Resources/ollama runner --model /x/blobs/sha256-abc --port 5000',
  '  300     1   300    512 /opt/old/ollama_llama_server --model y',
  '  400     1   400    999 /bin/zsh -c ollama runner is just text here',
  'garbage line',
].join('\n');

test('process table parsing keeps argv with spaces and converts KiB to bytes', () => {
  const rows = parseProcessTable(table);
  assert.equal(rows.length, 6);
  assert.deepEqual(rows[0], { pid: 100, ppid: 1, pgid: 100, rss_bytes: 2048 * 1024, args: '/usr/local/bin/llama-server --port 18080' });
  assert.equal(isOllamaRunner(rows[3].args), true);
  assert.equal(isOllamaRunner(rows[4].args), true);
  assert.equal(isOllamaRunner(rows[2].args), false);
  assert.equal(isOllamaRunner(rows[5].args), false);
});

test('RSS sums the owned process group and Ollama runners, counting a shared pid once', () => {
  const rows = parseProcessTable(table);
  const owned = summarizeProcessRss(rows, { ownedPgid: 100 });
  assert.equal(owned.owned_rss_bytes, 3072 * 1024);
  assert.deepEqual(owned.owned_pids, [100, 101]);
  assert.equal(owned.ollama_runner_rss_bytes, null);
  const ollama = summarizeProcessRss(rows, { includeOllamaRunners: true });
  assert.equal(ollama.ollama_runner_rss_bytes, (409600 + 512) * 1024);
  assert.equal(ollama.owned_rss_bytes, null);
  // An owned `ollama serve` group contains its runner: tracked RSS must not double count it.
  const both = summarizeProcessRss(rows, { ownedPgid: 200, includeOllamaRunners: true });
  assert.equal(both.tracked_rss_bytes, (4096 + 409600 + 512) * 1024);
  assert.equal(summarizeProcessRss(rows, {}).tracked_rss_bytes, null);
  assert.equal(summarizeProcessRss(rows, { ownedPgid: 999 }).owned_rss_bytes, 0);
});

test('RSS sampling runs ps only when something is tracked and keeps failures unknown', async () => {
  const calls = [];
  assert.equal(await sampleProcessRss({ execImpl: async (...args) => { calls.push(args); return { stdout: '' }; } }), null);
  assert.equal(calls.length, 0);
  const sample = await sampleProcessRss({ ownedPgid: 100, execImpl: async (file, args) => { calls.push([file, args]); return { stdout: table }; } });
  assert.deepEqual(calls[0], ['ps', ['-A', '-o', 'pid=,ppid=,pgid=,rss=,args=']]);
  assert.equal(sample.owned_rss_bytes, 3072 * 1024);
  const failed = await sampleProcessRss({ includeOllamaRunners: true, execImpl: async () => { throw new Error('denied'); } });
  assert.equal(failed.tracked_rss_bytes, null);
  assert.match(failed.error, /denied/);
  const peaks = summarizeRss([{ processes: { tracked_rss_bytes: 10, owned_rss_bytes: 4 } }, { processes: { tracked_rss_bytes: 30, owned_rss_bytes: 2 } }, { processes: null }, {}]);
  assert.equal(peaks.peak_tracked_bytes, 30);
  assert.equal(peaks.peak_owned_bytes, 4);
  assert.equal(peaks.peak_ollama_runner_bytes, null);
  assert.equal(peaks.samples_with_process_data, 2);
  assert.match(peaks.scope, /never added/);
});

test('macOS available memory uses the bounded core formula and is labelled an estimate', () => {
  assert.equal(macAvailableEstimate({ free: 10, inactive: 50, speculative: 5, fileBacked: 30, purgeable: 20 }), 40);
  assert.equal(macAvailableEstimate({ free: 10, inactive: 5, speculative: 5, fileBacked: 30, purgeable: 20 }), 30);
  assert.equal(macAvailableEstimate({ free: 10, inactive: 50, speculative: 5, fileBacked: 30, purgeable: 0, total: 25 }), 25);
  assert.equal(macAvailableEstimate({ free: 10, inactive: null, speculative: 5, fileBacked: 30, purgeable: 0 }), null);
  const vm = 'Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 100.\nPages active: 900.\nPages inactive: 400.\nPages speculative: 50.\nPages purgeable: 10.\nFile-backed pages: 300.\nPages occupied by compressor: 7.\n';
  const m = parseMacMemory(vm, 'total = 5G used = 1G free = 4G', '1', 64 * 1024 ** 3);
  assert.equal(m.available_bytes, (100 + 300) * 16384);
  assert.equal(m.available_bytes_source, 'freellama_estimate');
  assert.equal(m.free_bytes, 150 * 16384);
  assert.equal(parseMacMemory(vm, '', '', 1000).available_bytes, 1000);
  const summary = summarizeHost([{ timestamp_ms: 0, available_bytes: 9, available_bytes_source: 'freellama_estimate' }, { timestamp_ms: 1000, available_bytes: 7, available_bytes_source: 'freellama_estimate' }]);
  assert.equal(summary.min_available_bytes, 7);
  assert.equal(summary.available_bytes_source, 'freellama_estimate');
  assert.equal(Object.hasOwn(summary, 'process_rss'), false);
  assert.equal(summarizeHost([{ timestamp_ms: 0, available_bytes: null }]).available_bytes_source, null);
  assert.equal(summarizeHost([{ timestamp_ms: 0, processes: { tracked_rss_bytes: 5 } }]).process_rss.peak_tracked_bytes, 5);
});

test('top CPU parsing and the load gate warn or refuse only above the limit', async () => {
  const top = parseTopCpu('%CPU COMM\n 12.5 /Applications/Some App.app/Contents/MacOS/Some App\n 99.0 node\n  0.0 launchd\n 3.1 a\n 2.0 b\n 1.0 c\n');
  assert.equal(top.length, 5);
  assert.deepEqual(top[0], { pcpu: 99, command: 'node' });
  assert.equal(top[1].command, '/Applications/Some App.app/Contents/MacOS/Some App');
  const seen = [];
  const mac = await loadSnapshot({ platform: 'darwin', loadavg: [6, 5, 4], logicalCpus: 4, execImpl: async (file, args) => { seen.push(args); return { stdout: '%CPU COMM\n 50.0 x\n' }; } });
  const linux = await loadSnapshot({ platform: 'linux', loadavg: [1, 1, 1], logicalCpus: 4, execImpl: async (file, args) => { seen.push(args); return { stdout: '' }; } });
  assert.deepEqual(seen, [['-Ao', 'pcpu,comm', '-r'], ['-eo', 'pcpu,comm', '--sort=-pcpu']]);
  assert.equal(mac.load1_per_cpu, 1.5);
  assert.deepEqual(mac.top_processes, [{ pcpu: 50, command: 'x' }]);
  assert.equal(evaluateLoadGate(mac, 2).action, 'ok');
  assert.equal(evaluateLoadGate(mac, 1).action, 'warn');
  assert.equal(evaluateLoadGate(mac, 1, true).action, 'refuse');
  assert.equal(evaluateLoadGate(linux, 1, true).action, 'ok');
  assert.equal(evaluateLoadGate({ load1_per_cpu: null }, 1, true).action, 'unknown');
  const failed = await loadSnapshot({ loadavg: [1, 1, 1], logicalCpus: 1, execImpl: async () => { throw new Error('no ps'); } });
  assert.match(failed.error, /no ps/);
  assert.deepEqual(failed.top_processes, []);
});

test('residency reads size_vram from /api/ps and unload confirms absence there', async () => {
  let resident = true;
  const calls = [];
  const fetchImpl = async (url, options = {}) => {
    calls.push([new URL(url).pathname, options.method ?? 'GET', options.body]);
    if (url.endsWith('/api/generate')) { resident = false; return Response.json({ done: true, done_reason: 'unload' }); }
    return Response.json({ models: resident ? [{ name: 'fixture:1b', model: 'fixture:1b', size: 3000, size_vram: 2500, expires_at: 'later' }, { name: 'other:7b' }] : [{ name: 'other:7b' }] });
  };
  const before = await ollamaResidency('http://127.0.0.1:11434/', 'fixture:1b', { fetchImpl });
  assert.deepEqual([before.resident, before.size_bytes, before.size_vram_bytes, before.resident_models], [true, 3000, 2500, 2]);
  const outcome = await unloadOllamaModel('http://127.0.0.1:11434', 'fixture:1b', { fetchImpl, wait: async () => {} });
  assert.equal(outcome.confirmed_absent, true);
  assert.equal(outcome.http_status, 200);
  assert.deepEqual(JSON.parse(calls[1][2]), { model: 'fixture:1b', keep_alive: 0 });
  assert.equal(calls[1][1], 'POST');
  assert.equal(calls.at(-1)[0], '/api/ps');
  // A model that stays listed is reported, not assumed gone.
  let clock = 0;
  const stuck = await unloadOllamaModel('http://127.0.0.1:11434', 'other:7b', { fetchImpl, wait: async () => { clock += 1000; }, now: () => clock, confirmTimeoutMs: 3000 });
  assert.equal(stuck.confirmed_absent, false);
  assert.match(stuck.error, /still listed/);
  const down = await unloadOllamaModel('http://127.0.0.1:11434', 'x', { fetchImpl: async () => { throw new Error('ECONNREFUSED'); } });
  assert.equal(down.confirmed_absent, false);
  assert.match(down.error, /ECONNREFUSED/);
  assert.equal((await ollamaResidency('http://x', 'x', { fetchImpl: async () => new Response('nope', { status: 500 }) })).resident, null);
});
