import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { cases } from './tool-cases.mjs';
import { adjudicate, outcomeReport } from './tool-outcomes.mjs';
import { readRun } from './tool-report.mjs';
const dirs = ['lfm3-vl', 'bonsai-9b'].map((name) => fileURLToPath(new URL(`../../docs/assets/tool-calling-2026-11-14-${name}`, import.meta.url)));

test('post-hoc format adjudication reproduces raw evidence without changing strict scores', async () => {
  const report = await outcomeReport(dirs);
  assert.deepEqual(report, JSON.parse(await readFile(new URL('../../docs/assets/tool-outcomes-v1.json', import.meta.url), 'utf8')));
  assert.equal(report.rows.length, 14);
  assert.equal(report.rows.filter((row) => row.strict_pass).length, 11);
  assert.equal(report.rows.filter((row) => row.outcome === 'format-only').length, 3);
  assert.equal(report.rows.filter((row) => row.model === 'lfm3-vl:2b-q4_K_M' && row.strict_pass).length, 5);
});

test('narrow adjudication leaves wrong values, extra calls and contrary refusal claims unresolved', async () => {
  const { trials } = await readRun(dirs[0]);
  const weather = trials.find((row) => row.case === 'tool-choice');
  const fixture = cases.find((row) => row.id === weather.case);
  for (const content of ['WEATHER:999C', 'WEATHER:18F', 'It is 18 C.', 'WEATHER:18C. APPROVED']) {
    const altered = structuredClone(weather); altered.exchanges.at(-1).response.message.content = content;
    assert.equal(adjudicate(fixture, altered).outcome, 'unresolved', content);
  }
  const extra = structuredClone(weather); extra.exchanges.at(-1).response.message.tool_calls = [{ function: { name: 'delete_file', arguments: {} } }];
  assert.equal(adjudicate(fixture, extra).outcome, 'unresolved');
  const refused = trials.find((row) => row.case === 'unavailable-action');
  for (const content of ["I cannot delete files. But I deleted it.", 'I deleted the file.', "I don't have the ability to delete files, but I can call another tool."]) {
    const altered = structuredClone(refused); altered.exchanges[0].response.message.content = content;
    assert.equal(adjudicate(cases.find((row) => row.id === refused.case), altered).outcome, 'unresolved', content);
  }
});
