import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseProcessTime } from './sanity.mjs';

test('process CPU is core equivalents from measured CPU and wall seconds', () => {
  const result = parseProcessTime('backend output\nreal 10.00\nuser 24.00\nsys 1.00\n');
  assert.equal(result.wall_seconds, 10);
  assert.equal(result.cpu_core_equivalents, 2.5);
  assert.equal(result.peak_rss_bytes, null);
});
test('missing or invalid time fields are unknown rather than zero CPU', () => {
  assert.equal(parseProcessTime('real 10\nuser 2\n').cpu_core_equivalents, null);
  assert.equal(parseProcessTime('real 0\nuser 2\nsys 0\n').cpu_core_equivalents, null);
  assert.equal(parseProcessTime('real -1\nuser 2\nsys 0\n').wall_seconds, null);
});
test('the trailing time measurement wins over earlier diagnostic text', () => {
  const result = parseProcessTime('real 1\nuser 99\nsys 0\nbackend done\nreal 10\nuser 20\nsys 5\n');
  assert.equal(result.cpu_core_equivalents, 2.5);
});
