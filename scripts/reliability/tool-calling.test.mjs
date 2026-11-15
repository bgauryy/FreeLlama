import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { cases } from './tool-cases.mjs';
import { gradeFirst, gradeFinal, parseArgs, requestJson, run, summarize, renderTable } from './tool-calling.mjs';
const assistant = (content, calls = []) => ({ done: true, message: { role: 'assistant', content, tool_calls: calls } });
const call = (name, args) => ({ function: { name, arguments: args } });

test('tool grader checks identity, exact typed nested arguments and post-tool continuation', () => {
  for (const fixture of cases.filter((row) => row.expected)) {
    assert.equal(gradeFirst(fixture, assistant('', [call(fixture.expected.name, fixture.expected.args)])).pass, true);
    assert.equal(gradeFirst(fixture, assistant('', [call(fixture.expected.name, JSON.stringify(fixture.expected.args))])).pass, true);
    assert.equal(gradeFirst(fixture, assistant('', [call('invented', fixture.expected.args)])).pass, false);
    assert.equal(gradeFirst(fixture, assistant('', [call(fixture.expected.name, { ...fixture.expected.args, extra: 1 })])).pass, false);
    assert.equal(gradeFinal(fixture, assistant(fixture.final)), true);
    assert.equal(gradeFinal(fixture, assistant('APPROVED')), false);
    assert.equal(gradeFinal(fixture, assistant(fixture.final, [call(fixture.expected.name, fixture.expected.args)])), false);
    assert.deepEqual(gradeFirst(fixture, assistant('guessed')), { pass: false, tool_choice: false, arguments_valid: false, reason: 'expected one tool call, received 0' });
  }
  assert.equal(gradeFirst(cases[1], assistant('', [call('add', { a: '17', b: 25 })])).pass, false);
  assert.equal(gradeFirst(cases[0], { message: { role: 'assistant', content: '' } }).pass, false);
});

test('non-call cases reject unnecessary, invented and missing-information calls', () => {
  for (const fixture of cases.filter((row) => !row.expected)) {
    assert.equal(gradeFirst(fixture, assistant(fixture.final ?? 'Which city?')).pass, true);
    assert.equal(gradeFirst(fixture, assistant(fixture.final ?? 'Which city?', [call('get_weather', { city: 'Paris', unit: 'C' })])).pass, false);
  }
});

test('v2 grader rejects malformed call containers and city mentions without clarification', () => {
  const noCall = cases.find((row) => row.id === 'no-tool-needed');
  for (const invalid of [{ function: { name: 'add', arguments: {} } }, '[]', false]) {
    assert.equal(gradeFirst(noCall, assistant('READY', invalid)).pass, false);
    assert.equal(gradeFinal({ final: 'READY' }, assistant('READY', invalid)), false);
  }
  const clarify = cases.find((row) => row.id === 'missing-required-information');
  assert.equal(gradeFirst(clarify, assistant('The city is Paris.')).pass, false);
  assert.equal(gradeFirst(clarify, assistant('Which city?')).pass, true);
});

test('CLI requires explicit exact tags; bounds every request and never defaults to inference', () => {
  assert.throws(() => parseArgs(['--out', 'example']), /exact --model/);
  assert.equal(parseArgs(['--out', 'example', '--inventory']).models.length, 0);
  for (const args of [['--repeats', '0'], ['--max-tokens', '999999'], ['--timeout-ms', 'NaN'], ['--unknown', 'x']]) assert.throws(() => parseArgs(['--out', 'x', '--model', 'tag:exact', ...args]));
  assert.throws(() => parseArgs(['--out', 'x', '--model', 'tag:exact', '--endpoint', 'https://user:secret@example.com']), /credentials/);
  assert.throws(() => parseArgs(['--out', 'x', '--model', 'tag:exact', '--inventory']), /cannot generate/);
});

test('bounded HTTP rejects oversized and non-JSON audit responses', async () => {
  await assert.rejects(requestJson('http://test', '/api/chat', {}, { fetchImpl: async () => new Response('a'.repeat(2 * 1024 * 1024 + 1)) }), /2 MiB/);
  await assert.rejects(requestJson('http://test', '/api/chat', {}, { fetchImpl: async () => new Response('bad json') }), (error) => error.raw === 'bad json');
});

test('inventory never generates and unavailable exact tags never pull', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-reliability-'));
  try {
    const routes = [];
    const fetchImpl = async (url) => {
      const route = new URL(url).pathname; routes.push(route);
      return Response.json(route === '/api/tags' ? { models: [{ name: 'test:exact', digest: 'sha256:test', size: 1 }] } : {});
    };
    const report = await run(parseArgs(['--out', path.join(dir, 'inventory'), '--inventory']), { fetchImpl });
    assert.equal(report.models[0].status, 'unmeasured');
    assert.deepEqual(routes, ['/api/version', '/api/tags', '/api/ps', '/api/ps']);
    await assert.rejects(run(parseArgs(['--out', path.join(dir, 'missing'), '--model', 'test']), { fetchImpl }), /Exact installed tag/);
    assert.equal(routes.some((route) => /chat|generate|pull|delete/.test(route)), false);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('uncertain inference stops all further attempts and retains failed/planned denominator', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-reliability-'));
  try {
    let calls = 0;
    const fetchImpl = async (url, options) => {
      const route = new URL(url).pathname;
      if (route === '/api/tags') return Response.json({ models: [{ name: 'test:exact', digest: 'sha256:test', size: 1 }, { name: 'other:exact', digest: 'other', size: 1 }] });
      if (route === '/api/show') return Response.json({ capabilities: ['completion', 'tools'] });
      if (route === '/api/chat') { calls++; throw new Error('uncertain network failure'); }
      assert.equal(options.method, 'GET');
      return Response.json({});
    };
    const out = path.join(dir, 'run');
    const report = await run(parseArgs(['--out', out, '--model', 'test:exact']), { fetchImpl });
    assert.equal(calls, 1);
    assert.equal(report.models[0].status, 'incomplete');
    assert.equal(report.models[0].planned, cases.length);
    assert.equal(report.models[0].errors, 1);
    assert.equal(report.models[0].not_attempted, cases.length - 1);
    assert.equal(report.models[1].status, 'unmeasured');
    const raw = JSON.parse((await readFile(path.join(out, 'trials.jsonl'), 'utf8')).trim());
    assert.equal(raw.inference_state, 'unknown');
    assert.equal(raw.exchanges[0].request.stream, false);
    assert.equal(raw.exchanges[0].request.keep_alive, '60s');
    assert.equal(raw.exchanges[0].request.options.num_predict, 256);
    assert.match(renderTable(report), /0\/7/);
    await assert.rejects(run(parseArgs(['--out', out, '--inventory']), { fetchImpl }), /EEXIST/);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('successful trial audit preserves exact requests, replies and fake tool results', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-reliability-'));
  try {
    let chatCalls = 0;
    const fetchImpl = async (url, options) => {
      const route = new URL(url).pathname;
      if (route === '/api/tags') return Response.json({ models: [{ name: 'test:exact', digest: 'sha256:test', size: 1 }] });
      if (route === '/api/show') return Response.json({ capabilities: ['tools'] });
      if (route !== '/api/chat') return Response.json({});
      chatCalls++;
      const body = JSON.parse(options.body);
      const fixture = cases.find((row) => row.prompt === body.messages[1].content);
      if (body.messages.at(-1).role === 'tool') return Response.json(assistant(fixture.final));
      return Response.json(fixture.expected ? assistant('', [call(fixture.expected.name, fixture.expected.args)]) : assistant(fixture.final ?? 'Which city?'));
    };
    const out = path.join(dir, 'run');
    const report = await run(parseArgs(['--out', out, '--model', 'test:exact']), { fetchImpl });
    assert.equal(report.models[0].passed, cases.length);
    assert.equal(chatCalls, cases.length + cases.filter((row) => row.expected).length);
    const rows = (await readFile(path.join(out, 'trials.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
    assert.equal(rows[0].exchanges[1].request.messages.at(-1).content, JSON.stringify(cases[0].toolResult));
    assert.deepEqual(rows[0].exchanges[0].response.message.tool_calls[0].function.arguments, cases[0].expected.args);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('a failed continuation is a failed complete case and remains regradable', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-followup-error-'));
  try {
    let chats = 0;
    const fetchImpl = async (url) => {
      const route = new URL(url).pathname;
      if (route === '/api/tags') return Response.json({ models: [{ name: 'test:exact', digest: 'sha256:test', size: 1 }] });
      if (route === '/api/show') return Response.json({ capabilities: ['tools'] });
      if (route === '/api/chat') {
        if (++chats === 1) return Response.json(assistant('', [call(cases[0].expected.name, cases[0].expected.args)]));
        throw new Error('continuation transport failed');
      }
      return Response.json({});
    };
    const out = path.join(dir, 'run');
    const report = await run(parseArgs(['--out', out, '--model', 'test:exact']), { fetchImpl });
    assert.equal(report.models[0].passed, 0);
    assert.equal(report.models[0].errors, 1);
    const { readRun } = await import('./tool-report.mjs');
    const { trials } = await readRun(out);
    assert.equal(trials[0].first.pass, true);
    assert.equal(trials[0].pass, false);
    assert.equal(trials[0].exchanges.length, 2);
    assert.equal(chats, 2);
  } finally { await rm(dir, { recursive: true, force: true }); }
});
