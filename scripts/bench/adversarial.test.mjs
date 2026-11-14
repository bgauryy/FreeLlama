import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { mkdirSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { CASE, DEFAULT_LOCK_PATH, MAX_RESPONSE_BYTES, acquireLock, measureRequest, readBoundedBody, runBenchmark, validateConfig } from './lib.mjs';
import { fileURLToPath } from 'node:url';

const vendor = (id = 'a') => ({ id, protocol: 'ollama', base_url: `http://${id}.invalid`, model: 'fixture:1b' });
const answer = JSON.stringify(CASE.expected);
const complete = () => new Response(JSON.stringify({ done: true, message: { content: answer } }) + '\n');

test('ambiguous transport failure stops later samples and existing-service vendor blocks', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-unknown-'));
  let inferenceCalls = 0;
  try {
    const out = path.join(directory, 'out');
    const report = await runBenchmark({ config: { model: {}, vendors: [vendor('a'), vendor('b')] }, out, samples: 2, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: () => {}, fetchImpl: async url => {
      if (url.endsWith('/api/version')) return Response.json({ version: 'fixture' });
      if (url.endsWith('/api/tags')) return Response.json({ models: [{ name: 'fixture:1b' }] });
      inferenceCalls++;
      throw new Error('Connection lost after submission');
    } });
    assert.equal(inferenceCalls, 1);
    assert.equal(report.vendors[0].failed, 1);
    assert.equal(report.vendors[0].attempted, 1);
    assert.equal(report.vendors[1].status, 'blocked');
    assert.match(report.stop_reason, /uncertain inference/);
    assert.ok(report.finished_at);
    const trial = JSON.parse((await readFile(path.join(out, 'a/trials.jsonl'), 'utf8')).trim());
    assert.equal(trial.execution_uncertain, true);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('post-start refusal-looking responses remain uncertain failures', async () => {
  const result = await measureRequest(vendor(), {}, { fetchImpl: async () => Response.json({ code: 'admission_timeout', execution_outcome: 'unknown', retry_safe: false }, { status: 429 }) });
  assert.equal(result.outcome, 'failed');
  assert.equal(result.execution_uncertain, true);
  assert.equal(result.pre_execution_refusal, false);
});

test('stream content after a terminal marker is rejected', async () => {
  const wire = 'data: [DONE]\n\ndata: ' + JSON.stringify({ choices: [{ delta: { content: answer } }] }) + '\n\n';
  const result = await measureRequest({ ...vendor(), protocol: 'openai' }, {}, { fetchImpl: async () => new Response(wire) });
  assert.equal(result.outcome, 'failed');
  assert.match(result.error, /after its terminal/);
  assert.equal(result.raw_response, wire);
});

test('bounded capture cancels an oversized response and preserves the captured prefix', async () => {
  let cancelled = false;
  let prefix = '';
  const response = new Response(new ReadableStream({ start(controller) { controller.enqueue(new TextEncoder().encode('1234567890')); }, cancel() { cancelled = true; } }));
  await assert.rejects(readBoundedBody(response, { maxBytes: 5, onText: text => { prefix += text; } }), /exceeded 5 bytes/);
  assert.equal(prefix, '12345');
  assert.equal(cancelled, true);
  const measured = await measureRequest(vendor(), {}, { fetchImpl: async () => new Response('x'.repeat(MAX_RESPONSE_BYTES + 1)) });
  assert.equal(measured.outcome, 'failed');
  assert.equal(measured.execution_uncertain, true);
  assert.equal(measured.raw_response.length, MAX_RESPONSE_BYTES);
  assert.equal(measured.raw_response_truncated, true);
  const afterTerminal = await measureRequest(vendor(), {}, { fetchImpl: async () => new Response(JSON.stringify({ done: true, message: { content: answer } }) + '\n' + 'x'.repeat(MAX_RESPONSE_BYTES)) });
  assert.equal(afterTerminal.completed_stream, true);
  assert.equal(afterTerminal.execution_uncertain, true);
});

test('aborting an unresponsive response body ends the client wait and leaves execution uncertain', async () => {
  const controller = new AbortController();
  let cancelled = false;
  const result = await measureRequest(vendor(), {}, { signal: controller.signal, fetchImpl: async () => {
    queueMicrotask(() => controller.abort(new Error('Cancelled fixture')));
    return new Response(new ReadableStream({ cancel() { cancelled = true; } }));
  } });
  assert.equal(result.execution_uncertain, true);
  assert.match(result.error, /Cancelled fixture/);
  assert.equal(cancelled, true);
});

test('health and inference requests reject redirects rather than silently changing attribution', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-redirect-'));
  try {
    const seen = [];
    await runBenchmark({ config: { model: {}, vendors: [vendor()] }, out: path.join(directory, 'out'), samples: 1, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: () => {}, fetchImpl: async (url, options) => {
      seen.push(options.redirect);
      if (url.endsWith('/api/version')) return Response.json({ version: 'fixture' });
      if (url.endsWith('/api/tags')) return Response.json({ models: [{ name: 'fixture:1b' }] });
      return complete();
    } });
    assert.deepEqual(seen, ['error', 'error', 'error']);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('configuration rejects string booleans and unused secret-bearing fields before evidence is saved', () => {
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), enabled: 'false' }] }), /must be a boolean/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), api_key: 'private-fixture' }] }), /Unknown vendor field: api_key/);
  assert.throws(() => validateConfig({ model: { token: 'private-fixture' }, vendors: [vendor()] }), /Unknown model field: token/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), start: { command: 'fixture', args: [] } }] }, { platform: 'win32' }), /POSIX process-group/);
});

test('cancellation before a vendor starts is recorded in a finished report', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-cancel-'));
  try {
    const controller = new AbortController(); controller.abort(new Error('Cancelled fixture'));
    let calls = 0;
    const report = await runBenchmark({ config: { model: {}, vendors: [vendor()] }, out: path.join(directory, 'out'), samples: 1, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), signal: controller.signal, onProgress: () => {}, fetchImpl: async () => { calls++; return complete(); } });
    assert.equal(calls, 0);
    assert.equal(report.vendors[0].status, 'blocked');
    assert.match(report.stop_reason, /Cancelled fixture/);
    assert.ok(report.finished_at);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('an uncertain owned frontend still blocks later vendors after successful teardown', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-owned-'));
  let stopped = false, inferenceCalls = 0;
  const first = { ...vendor('a'), protocol: 'freellama-managed', start: { command: 'fixture', args: [] } };
  try {
    const report = await runBenchmark({ config: { model: {}, vendors: [first, vendor('b')] }, out: path.join(directory, 'out'), samples: 2, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: () => {}, startProcessImpl: async () => ({ child: { pid: 123 }, assertRunning() {}, async stop() { await Promise.resolve(); stopped = true; } }), fetchImpl: async url => {
      const parsed = new URL(url);
      if (parsed.pathname === '/') throw new Error('No listener before fixture start');
      if (url.endsWith('/ready')) return Response.json({ ready: true });
      if (url.endsWith('/api/version')) return Response.json({ version: 'fixture' });
      if (url.endsWith('/api/tags')) return Response.json({ models: [{ name: 'fixture:1b' }] });
      inferenceCalls++;
      if (parsed.hostname === 'a.invalid') throw new Error('Connection lost after submission');
      return complete();
    } });
    assert.equal(stopped, true);
    assert.equal(inferenceCalls, 1);
    assert.equal(report.vendors[0].attempted, 1);
    assert.equal(report.vendors[1].status, 'blocked');
    assert.match(report.stop_reason, /frontend teardown cannot establish upstream quiescence/);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('uncertainty halts later inference even when trial persistence fails', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-uncertain-write-'));
  const out = path.join(directory, 'out');
  let inferenceCalls = 0;
  try {
    const report = await runBenchmark({ config: { model: {}, vendors: [vendor('a'), vendor('b')] }, out, samples: 2, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: message => {
      if (message.startsWith('Starting a;')) mkdirSync(path.join(out, 'a/trials.jsonl'));
    }, fetchImpl: async url => {
      if (url.endsWith('/api/version')) return Response.json({ version: 'fixture' });
      if (url.endsWith('/api/tags')) return Response.json({ models: [{ name: 'fixture:1b' }] });
      inferenceCalls++; throw new Error('Connection lost after submission');
    } });
    assert.equal(inferenceCalls, 1);
    assert.equal(report.vendors[1].status, 'blocked');
    assert.match(report.stop_reason, /uncertain inference/);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('native frames cannot attribute a correct answer to an explicitly different model', async () => {
  for (const protocol of ['ollama', 'freellama']) {
    const wire = JSON.stringify({ model: 'wrong:7b', message: { content: answer } }) + '\n' + JSON.stringify({ model: 'fixture:1b', done: true }) + '\n';
    const result = await measureRequest({ ...vendor(), protocol }, {}, { fetchImpl: async () => new Response(wire) });
    assert.equal(result.outcome, 'failed');
    assert.equal(result.completed_stream, true);
    assert.equal(result.execution_uncertain, false);
    assert.equal(result.raw_response, wire);
    assert.match(result.error, /different model/);
  }
});

test('managed pinned attribution requires a route and rejects a contradictory response model', async () => {
  const managed = { ...vendor(), protocol: 'freellama-managed' };
  for (const value of [
    { response: { model: 'fixture:1b', done: true, message: { content: answer } } },
    { route: { selected_model: 'fixture:1b' }, response: { model: 'wrong:7b', done: true, message: { content: answer } } },
  ]) {
    const result = await measureRequest(managed, {}, { fetchImpl: async () => Response.json(value) });
    assert.equal(result.outcome, 'failed');
    assert.equal(result.execution_uncertain, false);
    assert.deepEqual(result.receipt, value);
    assert.match(result.error, /model/);
  }
});

test('negative native and OpenAI usage or durations remain unknown rather than becoming KPI values', async () => {
  const native = await measureRequest(vendor(), {}, { fetchImpl: async () => new Response(JSON.stringify({ model: 'fixture:1b', done: true, message: { content: answer }, eval_count: -5, eval_duration: 1e9, prompt_eval_count: -3, load_duration: -20, prompt_eval_duration: -6 }) + '\n') });
  assert.equal(native.outcome, 'verified');
  assert.deepEqual(native.metrics, { output_tokens: null, prompt_tokens: null, load_ms: null, prompt_eval_ms: null, output_tokens_per_second: null });
  const wire = 'data: ' + JSON.stringify({ choices: [{ delta: { content: answer } }], usage: { completion_tokens: -2, prompt_tokens: -4 } }) + '\n\ndata: [DONE]\n\n';
  const openai = await measureRequest({ ...vendor(), protocol: 'openai' }, {}, { fetchImpl: async () => new Response(wire) });
  assert.equal(openai.outcome, 'verified');
  assert.equal(openai.metrics.output_tokens, null);
  assert.equal(openai.metrics.prompt_tokens, null);
});

test('failed owned-server teardown blocks every later vendor', async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-cleanup-'));
  let inferenceCalls = 0;
  try {
    const report = await runBenchmark({ config: { model: {}, vendors: [{ ...vendor('a'), start: { command: 'fixture', args: [] } }, vendor('b')] }, out: path.join(directory, 'out'), samples: 1, warmups: 0, allowSmoke: true, lockPath: path.join(directory, 'lock'), onProgress: () => {}, startProcessImpl: async () => ({ child: { pid: 123 }, assertRunning() {}, async stop() { throw new Error('Fixture child still alive'); } }), fetchImpl: async url => {
      if (new URL(url).pathname === '/') throw new Error('No listener before fixture start');
      if (url.endsWith('/api/version')) return Response.json({ version: 'fixture' });
      if (url.endsWith('/api/tags')) return Response.json({ models: [{ name: 'fixture:1b' }] });
      inferenceCalls++; return complete();
    } });
    assert.equal(inferenceCalls, 1);
    assert.equal(report.vendors[0].status, 'failed');
    assert.equal(report.vendors[1].status, 'blocked');
    assert.match(report.vendors[0].cleanup_error, /still alive/);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('checkout lock identity survives cwd changes and old releases cannot remove a new owner', async () => {
  assert.equal(DEFAULT_LOCK_PATH, fileURLToPath(new URL('../../.octocode/benchmark.lock', import.meta.url)));
  const directory = await mkdtemp(path.join(os.tmpdir(), 'freellama-bench-lock-owner-'));
  try {
    const lock = path.join(directory, 'lock');
    const release = await acquireLock(lock);
    await release();
    const second = await acquireLock(lock);
    await release();
    await assert.rejects(acquireLock(lock), /Another benchmark/);
    await writeFile(path.join(lock, 'owner.json'), JSON.stringify({ token: 'replacement-owner' }));
    await assert.rejects(second(), /ownership changed/);
    assert.equal(JSON.parse(await readFile(path.join(lock, 'owner.json'), 'utf8')).token, 'replacement-owner');
  } finally { await rm(directory, { recursive: true, force: true }); }
});
