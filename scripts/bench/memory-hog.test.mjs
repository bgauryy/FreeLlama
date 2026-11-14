import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { GIB, allocateTouched, hogDecision, parseHogArgs, parseHogGb, startMemoryHog } from './memory-hog.mjs';

const script = fileURLToPath(new URL('./memory-hog.mjs', import.meta.url));
const MiB = 1024 ** 2;

test('hog limit refuses above estimated available minus 2 GiB, or unknown memory, unless forced', () => {
  const fits = hogDecision({ gb: 1, availableBytes: 4 * GIB });
  assert.equal(fits.allowed, true);
  assert.equal(fits.forced, false);
  assert.equal(fits.limit_bytes, 2 * GIB);
  const exact = hogDecision({ gb: 2, availableBytes: 4 * GIB });
  assert.equal(exact.allowed, true);
  const over = hogDecision({ gb: 2.5, availableBytes: 4 * GIB });
  assert.equal(over.allowed, false);
  assert.match(over.reason, /exceeds estimated available minus 2 GiB/);
  assert.equal(hogDecision({ gb: 1, availableBytes: GIB }).limit_bytes, 0);
  const unknown = hogDecision({ gb: 0.5, availableBytes: null });
  assert.equal(unknown.allowed, false);
  assert.match(unknown.reason, /unknown/);
  const forced = hogDecision({ gb: 2.5, availableBytes: 4 * GIB, force: true });
  assert.equal(forced.allowed, true);
  assert.equal(forced.forced, true);
  for (const value of [0, -1, 'x', '', undefined, 2000]) assert.throws(() => parseHogGb(value), /--hog-gb/);
  assert.deepEqual(parseHogArgs(['--gb', '0.01', '--force']), { gb: 0.01, force: true });
  assert.throws(() => parseHogArgs(['--force']), /--gb is required/);
  assert.throws(() => parseHogArgs(['--gb', '1', '--extra']), /Unknown argument/);
});

test('allocation touches every byte in bounded chunks', () => {
  const chunks = allocateTouched(5 * MiB + 3, 2 * MiB);
  assert.deepEqual(chunks.map(chunk => chunk.length), [2 * MiB, 2 * MiB, MiB + 3]);
  assert.ok(chunks.every(chunk => chunk[0] === 0x5a && chunk.at(-1) === 0x5a));
});

test('the helper refuses an impossible size before allocating anything', async () => {
  const run = promisify(execFile);
  await assert.rejects(run(process.execPath, [script, '--gb', '1000'], { timeout: 20000 }), error => {
    assert.equal(error.code, 3);
    assert.equal(JSON.parse(error.stderr.trim()).refused, true);
    return true;
  });
});

test('runner lifecycle: tiny forced hog becomes ready, holds, and stops on SIGTERM', async () => {
  const hog = await startMemoryHog({ gb: 0.01, force: true, timeoutMs: 20000 });
  assert.equal(hog.ready.ready, true);
  assert.equal(hog.ready.bytes, Math.ceil(0.01 * GIB));
  assert.ok(hog.ready.bytes < 64 * MiB);
  assert.equal(typeof hog.pid, 'number');
  process.kill(hog.pid, 0);
  const stopped = await hog.stop();
  assert.equal(stopped.killed, undefined);
  assert.throws(() => process.kill(hog.pid, 0), { code: 'ESRCH' });
  await assert.rejects(startMemoryHog({ gb: 1000, timeoutMs: 20000 }), /exited before ready/);
});
