import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { readFile, mkdtemp, cp, rm } from 'node:fs/promises';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { combinedTable, readRun, verifyRequests, runIdentity } from './tool-report.mjs';
import { cases } from './tool-cases.mjs';
const dirs = ['lfm3-vl', 'bonsai-9b'].map((name) => fileURLToPath(new URL(`../../docs/assets/tool-calling-2026-11-14-${name}`, import.meta.url)));

test('published local model evidence regrades every raw response and reproduces the public table', async () => {
  assert.equal((await readRun(dirs[0])).summary.models.find((row) => row.model === 'lfm3-vl:2b-q4_K_M').passed, 5);
  assert.equal((await readRun(dirs[1])).summary.models.find((row) => row.model === 'bonsai-2:9b').passed, 6);
  assert.equal(await combinedTable(dirs), await readFile(new URL('../../docs/assets/tool-calling-table.md', import.meta.url), 'utf8'));
});

test('request audit detects changed prompts, schemas, limits and tool-result histories', async () => {
  const { manifest, trials } = await readRun(dirs[0]);
  const row = trials[0], fixture = cases.find((entry) => entry.id === row.case);
  for (const change of [
    (copy) => { copy.exchanges[0].request.messages[1].content = 'Reply anything'; },
    (copy) => { copy.exchanges[0].request.tools = []; },
    (copy) => { copy.exchanges[0].request.options.num_predict = 1; },
    (copy) => { copy.exchanges[1].request.messages.at(-1).content = '{"temperature":999}'; },
    (copy) => { copy.exchanges[1].request.messages[2].content = 'a different assistant'; },
  ]) {
    const altered = structuredClone(row); change(altered);
    assert.throws(() => verifyRequests(manifest, altered, fixture), /Request provenance mismatch/);
  }
  const overridden = structuredClone(row);
  overridden.exchanges.forEach((exchange) => { exchange.request.options.num_predict = 128; });
  assert.doesNotThrow(() => verifyRequests({ ...manifest, options: { ...manifest.options, maxTokens: 128 } }, overridden, fixture));
});

test('copied run directories cannot multiply acquired evidence', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-report-copy-'));
  try {
    const copy = path.join(dir, 'copy'); await cp(dirs[0], copy, { recursive: true });
    await assert.rejects(combinedTable([dirs[0], copy]), /Duplicate acquisition/);
    const { manifest, trials } = await readRun(dirs[0]);
    assert.equal(runIdentity(manifest, trials), runIdentity({ ...manifest, provenance: { ...manifest.provenance, dirty_files: 'incidental metadata' } }, trials));
  } finally { await rm(dir, { recursive: true, force: true }); }
});
