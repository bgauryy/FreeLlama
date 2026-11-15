import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { evaluateChecks, confusion } from './result-check-accuracy.mjs';

test('labeled check corpus reports false positives, missed risks, and reproducible evidence', async () => {
  const report = await evaluateChecks();
  const baseline = JSON.parse(await readFile(new URL('../../docs/assets/result-check-accuracy.json', import.meta.url), 'utf8'));
  const changed = new Set(['source-formatting', 'source-substring-collision']);
  for (const row of report.rows) {
    const prior = baseline.rows.find((item) => item.id === row.id);
    assert.equal(row.risk, prior.risk, `${row.id}: human risk label is unchanged`);
    assert.equal(row.baseline_warning, prior.warning, row.id);
    assert.equal(row.warning, changed.has(row.id) ? !prior.warning : prior.warning, row.id);
  }
  assert.deepEqual(report.families['source-factual-risk'], { tp: 3, fp: 1, tn: 3, fn: 5, precision: 0.75, recall: 0.375 });
  assert.deepEqual(report, JSON.parse(await readFile(new URL('../../docs/assets/result-check-accuracy-v2.json', import.meta.url), 'utf8')));
  assert.equal(report.rows.length, 30);
});

test('empty denominators remain unknown, rather than perfect accuracy', () => {
  assert.deepEqual(confusion([]), { tp: 0, fp: 0, tn: 0, fn: 0, precision: null, recall: null });
});
