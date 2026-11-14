import { test } from 'node:test';
import assert from 'node:assert/strict';
import { probeLoopback } from './preflight-http.mjs';

test('health GET reaches the injected transport and returns parsed endpoint data', async () => {
  let observed;
  const result = await probeLoopback('http://127.0.0.1:11434/', '/api/version', undefined, {
    fetchImpl: async (url, options) => {
      observed = { url, options };
      return new Response(JSON.stringify({ version: 'fixture' }), { status: 200 });
    },
  });
  assert.equal(observed.url, 'http://127.0.0.1:11434/api/version');
  assert.equal(observed.options.method, 'GET');
  assert.equal(observed.options.body, undefined);
  assert.equal(observed.options.redirect, 'error');
  assert.ok(observed.options.signal instanceof AbortSignal);
  assert.deepEqual(result, { ok: true, status: 200, json: { version: 'fixture' }, text: undefined });
});

test('decision-only POST serializes the request independently of response parsing', async () => {
  const preview = { task: 'completion', model: 'fixture:tag', context_tokens: 4096 };
  let observed;
  const result = await probeLoopback('http://localhost:11435', '/_freellama/v1/routes', preview, {
    fetchImpl: async (url, options) => {
      observed = { url, options };
      return new Response(JSON.stringify({ execution: { resource_assessment: { status: 'ready' } } }), { status: 200 });
    },
  });
  assert.equal(observed.options.method, 'POST');
  assert.deepEqual(observed.options.headers, { 'content-type': 'application/json' });
  assert.deepEqual(JSON.parse(observed.options.body), preview);
  assert.equal(result.json.execution.resource_assessment.status, 'ready');
  assert.equal(result.ok, true);
});

test('invalid or nonloopback endpoints are refused before transport', async () => {
  let calls = 0;
  const fetchImpl = async () => { calls++; throw Error('Must not reach transport'); };
  for (const base of ['https://example.org', 'http://user:secret@localhost', 'file:///tmp/test']) {
    const result = await probeLoopback(base, '/api/version', undefined, { fetchImpl });
    assert.equal(result.ok, false);
    assert.match(result.error, /Only local loopback/);
  }
  assert.equal(calls, 0);
});

test('HTTP refusal text and transport diagnostics are retained', async () => {
  const refused = await probeLoopback('http://127.0.0.1:11435', '/_freellama/v1/ready', undefined, {
    fetchImpl: async () => new Response('not ready', { status: 503 }),
  });
  assert.deepEqual(refused, { ok: false, status: 503, json: null, text: 'not ready' });
  const disconnected = await probeLoopback('http://127.0.0.1:11434', '/api/version', undefined, {
    fetchImpl: async () => { throw Object.assign(Error('fetch failed'), { cause: { code: 'ECONNREFUSED' } }); },
  });
  assert.deepEqual(disconnected, { ok: false, error: 'fetch failed (ECONNREFUSED)' });
});

test('falsy JSON remains parsed JSON rather than fallback text', async () => {
  const result = await probeLoopback('http://[::1]:11434', '/api/version', undefined, {
    fetchImpl: async () => new Response('false', { status: 200 }),
  });
  assert.deepEqual(result, { ok: true, status: 200, json: false, text: undefined });
});

test('origin paths and URL query/fragment cannot redirect a health probe away from its route', async () => {
  let calls = 0;
  const fetchImpl = async () => { calls++; throw Error('Must not reach transport'); };
  for (const base of ['http://localhost:11434/other', 'http://localhost:11434/?x=1', 'http://localhost:11434/#fragment']) {
    const result = await probeLoopback(base, '/api/version', undefined, { fetchImpl });
    assert.equal(result.ok, false);
    assert.match(result.error, /must be an origin/);
  }
  assert.equal(calls, 0);
});

test('health response body is bounded during consumption and cancels the stream', async () => {
  let cancelled = false;
  const response = new Response(new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode('0123456789')); },
    cancel() { cancelled = true; },
  }));
  const result = await probeLoopback('http://localhost:11434', '/api/version', undefined, { fetchImpl: async () => response, maxResponseBytes: 4 });
  assert.equal(result.ok, false);
  assert.match(result.error, /exceeds 4 bytes/);
  assert.equal(cancelled, true);
});

test('an explicit falsy request body still sends POST rather than silently becoming GET', async () => {
  let options;
  const result = await probeLoopback('http://localhost:11434', '/preview', false, { fetchImpl: async (_url, observed) => { options = observed; return new Response('{}'); } });
  assert.equal(result.ok, true);
  assert.equal(options.method, 'POST');
  assert.equal(options.body, 'false');
});
