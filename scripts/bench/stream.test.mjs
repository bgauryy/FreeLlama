import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { CASE, measureRequest, requestFor, summarizeVendor } from './lib.mjs';

const answer = JSON.stringify(CASE.expected);
const receipt = (content = answer) => ({
  route: { task: 'completion', selected_model: 'fixture:1b' },
  admission: { mode: 'managed', queue_wait_ms: 4, total_wait_ms: 6 },
  execution: { backend: 'gpu', observation: { status: 'verified' } },
  metrics: { prompt_tokens: 80, output_tokens: 20, load_duration_ns: 3e6, output_tokens_per_second: 40 },
  timing: { elapsed_ms: 50 },
  response: { done: true, message: { role: 'assistant', content, thinking: '' } },
});
const sse = (event, data) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

// Serves `chunks` as separate TCP writes with a pause between them.
async function withServer(handler, run) {
  const server = createServer(handler);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try { return await run({ id: 'freellama-stream', protocol: 'freellama-stream', base_url: `http://127.0.0.1:${server.address().port}`, model: 'fixture:1b' }); }
  finally { server.closeAllConnections?.(); await new Promise(resolve => server.close(resolve)); }
}
const streaming = (chunks, seen = {}) => async (req, res) => {
  let body = ''; for await (const part of req) body += part;
  seen.url = req.url; seen.body = JSON.parse(body);
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  for (const chunk of chunks) { res.write(chunk); await pause(15); }
  res.end();
};

test('stream protocol sends the managed body plus stream:true to the tasks endpoint', () => {
  const managed = requestFor({ id: 'm', protocol: 'freellama-managed', base_url: 'http://x.invalid', model: 'fixture:1b' }, { context_tokens: 2048, output_tokens: 32 });
  const streamed = requestFor({ id: 's', protocol: 'freellama-stream', base_url: 'http://x.invalid', model: 'fixture:1b' }, { context_tokens: 2048, output_tokens: 32 });
  assert.equal(streamed.url, managed.url);
  assert.deepEqual(streamed.body, { ...managed.body, stream: true });
  assert.equal(Object.hasOwn(managed.body, 'stream'), false);
  assert.match(summarizeVendor({ id: 's', protocol: 'freellama-stream' }, []).mode, /streamed/);
});

test('SSE split across TCP writes: TTFT is the first content delta and grading uses done', async () => {
  const wire = sse('admission', { status: 'queued', reason: 'waiting_for_slot' }) + sse('admission', { status: 'admitted', reason: 'capacity_available', detail: { queue_wait_ms: 4 } })
    + sse('route', { selected_model: 'fixture:1b', backend: 'gpu' }) + sse('delta', { thinking: 'hmm' }) + sse('delta', { content: '' })
    + sse('delta', { content: answer.slice(0, 7) }) + sse('delta', { content: answer.slice(7) }) + `: keepalive\n\n` + sse('done', receipt());
  // Cut inside field names, inside JSON and between \n\n so the parser must buffer.
  const cuts = [3, 18, 61, 140, 141, 230, 300, wire.length - 40, wire.length];
  const chunks = cuts.map((end, index) => wire.slice(index ? cuts[index - 1] : 0, end));
  const seen = {};
  const result = await withServer(streaming(chunks, seen), vendor => measureRequest(vendor, {}));
  assert.equal(seen.url, '/_freellama/v1/tasks');
  assert.equal(seen.body.stream, true);
  assert.equal(seen.body.task, 'completion');
  assert.equal(result.outcome, 'verified');
  assert.equal(result.raw_response, wire);
  assert.equal(result.content, answer);
  assert.equal(result.completed_stream, true);
  assert.equal(result.response_complete, true);
  assert.equal(result.execution_uncertain, false);
  assert.ok(result.first_content_ms > 0 && result.first_content_ms < result.elapsed_ms);
  assert.deepEqual(result.receipt, receipt());
  assert.equal(result.admission.queue_wait_ms, 4);
  assert.equal(result.metrics.load_ms, 3);
  assert.deepEqual(result.stream.admission.map(event => event.status), ['queued', 'admitted']);
  assert.deepEqual(result.stream.route, { selected_model: 'fixture:1b', backend: 'gpu' });
  assert.deepEqual(result.stream.events, { admission: 2, route: 1, delta: 4, done: 1 });
  assert.equal(result.stream.streamed_thinking, 'hmm');
  assert.equal(result.stream.streamed_content_matches_done, true);
  assert.equal(result.stream.terminal_event, 'done');
});

test('done payload is graded exactly as managed, even when streamed deltas looked right', async () => {
  const wrong = JSON.stringify({ ...CASE.expected, priority: 'low' });
  const wire = sse('delta', { content: answer }) + sse('done', receipt(wrong));
  const result = await withServer(streaming([wire.slice(0, 20), wire.slice(20)]), vendor => measureRequest(vendor, {}));
  assert.equal(result.outcome, 'quality_failed');
  assert.equal(result.stream.streamed_content_matches_done, false);
  const otherModel = receipt(); otherModel.route.selected_model = 'other:7b';
  const misattributed = await withServer(streaming([sse('done', otherModel)]), vendor => measureRequest(vendor, {}));
  assert.equal(misattributed.outcome, 'failed');
  assert.match(misattributed.error, /different model/);
});

test('error events: a safe refusal stays a refusal; unsafe or truncated streams stay uncertain', async () => {
  const refusal = await withServer(streaming([sse('admission', { status: 'refused', reason: 'memory' }) + sse('error', { code: 'resource_admission_unavailable', error: 'no memory', execution_outcome: 'not_started', retry_safe: true })]), vendor => measureRequest(vendor, {}));
  assert.equal(refusal.outcome, 'refused');
  assert.equal(refusal.pre_execution_refusal, true);
  assert.equal(refusal.execution_uncertain, false);
  assert.equal(refusal.stream.terminal_event, 'error');
  const unsafe = await withServer(streaming([sse('delta', { content: '{' }) + sse('error', { code: 'execution_outcome_unknown', error: 'upstream lost', execution_outcome: 'unknown', retry_safe: false })]), vendor => measureRequest(vendor, {}));
  assert.equal(unsafe.outcome, 'failed');
  assert.equal(unsafe.execution_uncertain, true);
  const truncated = await withServer(streaming([sse('delta', { content: answer })]), vendor => measureRequest(vendor, {}));
  assert.equal(truncated.outcome, 'failed');
  assert.match(truncated.error, /Truncated stream/);
  assert.equal(truncated.execution_uncertain, true);
  const late = await withServer(streaming([sse('done', receipt()) + sse('delta', { content: 'x' })]), vendor => measureRequest(vendor, {}));
  assert.equal(late.outcome, 'failed');
  assert.match(late.error, /after its terminal/);
  const malformed = await withServer(streaming(['event: delta\ndata: {bad\n\n', sse('done', receipt())]), vendor => measureRequest(vendor, {}));
  assert.equal(malformed.outcome, 'failed');
  assert.match(malformed.error, /Malformed delta event/);
});

test('a JSON refusal before streaming starts is read as a managed receipt', async () => {
  const result = await withServer(async (req, res) => {
    for await (const part of req) void part;
    res.writeHead(503, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ code: 'admission_queue_full', error: 'busy', execution_outcome: 'not_started', retry_safe: true }));
  }, vendor => measureRequest(vendor, {}));
  assert.equal(result.outcome, 'refused');
  assert.equal(result.stream.non_sse_response, true);
  assert.equal(result.first_content_ms, null);
});

test('config accepts the stream protocol and an Ollama origin; the example keeps streaming disabled', async () => {
  const { validateConfig } = await import('./lib.mjs');
  const { validatePreflightConfig } = await import('./preflight-checks.mjs');
  const example = JSON.parse(await (await import('node:fs/promises')).readFile(new URL('./vendors.example.json', import.meta.url), 'utf8'));
  assert.doesNotThrow(() => validateConfig(example));
  assert.doesNotThrow(() => validatePreflightConfig(example));
  const stream = example.vendors.find(vendor => vendor.protocol === 'freellama-stream');
  assert.equal(stream.enabled, false);
  assert.doesNotThrow(() => validateConfig({ ollama_base_url: 'http://127.0.0.1:11434', model: { ollama_tag: 'x:1' }, vendors: [] }));
  for (const bad of ['http://127.0.0.1:11434/api', 'http://user:pw@127.0.0.1', 'ftp://127.0.0.1']) assert.throws(() => validateConfig({ ollama_base_url: bad, model: {}, vendors: [] }), /ollama_base_url/);
});
