import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';
import { createHash } from 'node:crypto';
import { cases, suiteVersion } from './tool-cases.mjs';
import { gradeFirst, gradeFinal, gradeFirstV1, gradeFinalV1, STRICT_GRADER_VERSION, chatRequest, followupRequest, summarize, renderTable } from './tool-calling.mjs';

const digest = (value) => createHash('sha256').update(value).digest('hex');
export function runIdentity(manifest, trials) {
  // The acquisition identity survives directory copies, aliases and incidental metadata edits.
  return digest(JSON.stringify([manifest.run_id ?? null, manifest.started_at, manifest.endpoint,
    trials.map(({ model, digest: modelDigest, case: testCase, repeat, started_at }) => [model, modelDigest, testCase, repeat, started_at])]));
}
function graders(manifest) {
  if (manifest.grader_version === undefined || manifest.grader_version === 'tool-strict-v1') return [gradeFirstV1, gradeFinalV1];
  if (manifest.grader_version === STRICT_GRADER_VERSION) return [gradeFirst, gradeFinal];
  throw new Error(`Unsupported strict grader ${manifest.grader_version}`);
}
export function verifyRequests(manifest, row, fixture) {
  const [firstGrade] = graders(manifest);
  const initial = chatRequest(row.model, fixture, manifest.options);
  const exchanges = row.exchanges;
  if (!Array.isArray(exchanges) || exchanges.length < 1 || !isDeepStrictEqual(exchanges[0]?.request, initial)) throw new Error(`Request provenance mismatch: ${row.case} initial prompt/tools/options`);
  const expectsFollowup = fixture.expected && firstGrade(fixture, exchanges[0]?.response).pass;
  const expectedCount = expectsFollowup ? 2 : 1;
  if (exchanges.length !== expectedCount) throw new Error(`Request provenance mismatch: ${row.case} exchange count`);
  if (expectsFollowup && !isDeepStrictEqual(exchanges[1]?.request, followupRequest(initial, fixture, exchanges[0].response))) throw new Error(`Request provenance mismatch: ${row.case} assistant/tool-result continuation`);
}

export async function readRun(dir) {
  const manifest = JSON.parse(await readFile(path.join(dir, 'manifest.json'), 'utf8'));
  let text = '';
  try { text = await readFile(path.join(dir, 'trials.jsonl'), 'utf8'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  const trials = text.trim() ? text.trim().split('\n').map(JSON.parse) : [];
  if (manifest.suite_version !== suiteVersion) throw new Error(`Unsupported suite in ${dir}`);
  const casesHash = digest(await readFile(new URL('./tool-cases.mjs', import.meta.url)));
  if (manifest.provenance?.cases_sha256 !== casesHash) throw new Error(`Case provenance mismatch in ${dir}`);
  for (const [name, max] of [['maxTokens', 2048], ['repeats', 100], ['timeoutMs', 300000]]) {
    const value = manifest.options?.[name];
    if (!Number.isInteger(value) || value < 1 || value > max) throw new Error(`Invalid recorded ${name} in ${dir}`);
  }
  const [firstGrade, finalGrade] = graders(manifest);
  const seen = new Set();
  for (const row of trials) {
    const fixture = cases.find((entry) => entry.id === row.case);
    const identity = `${row.model}\0${row.case}\0${row.repeat}`;
    if (!fixture || seen.has(identity) || !Number.isInteger(row.repeat) || row.repeat < 0 || row.repeat >= manifest.options.repeats
      || !manifest.options.models.includes(row.model) || !manifest.inventory.some((model) => model.name === row.model && model.digest === row.digest)) throw new Error(`Invalid or duplicate trial in ${dir}`);
    seen.add(identity);
    verifyRequests(manifest, row, fixture);
    if (row.error) { if (row.pass) throw new Error(`Error marked pass in ${dir}`); continue; }
    const first = firstGrade(fixture, row.exchanges[0]?.response);
    const follow = fixture.expected && first.pass ? finalGrade(fixture, row.exchanges[1]?.response) : undefined;
    if (!isDeepStrictEqual(first, row.first) || (follow ?? first.pass) !== row.pass || follow !== row.followup) throw new Error(`Grader mismatch ${row.case} in ${dir}`);
  }
  const summary = summarize(manifest, trials);
  const saved = JSON.parse(await readFile(path.join(dir, 'summary.json'), 'utf8'));
  if (!isDeepStrictEqual(summary, saved)) throw new Error(`Summary mismatch in ${dir}`);
  return { manifest, summary, trials, run_identity: runIdentity(manifest, trials) };
}

export async function combinedTable(dirs) {
  const runs = [];
  for (const dir of dirs) runs.push(await readRun(dir));
  if (!runs.length) throw new Error('At least one run is required');
  if (new Set(runs.map((run) => run.run_identity)).size !== runs.length) throw new Error('Duplicate acquisition: copied or repeated runs cannot add evidence');
  const signature = ({ manifest }) => JSON.stringify([manifest.suite_version, manifest.provenance.harness_sha256, manifest.provenance.cases_sha256,
    manifest.version, manifest.provenance.node, manifest.provenance.platform, manifest.provenance.arch, manifest.options.maxTokens, manifest.options.timeoutMs, manifest.grader_version ?? 'tool-strict-v1']);
  if (runs.some((run) => signature(run) !== signature(runs[0]))) throw new Error('Cannot pool different harness, suite, runtime or request settings');
  const models = new Map();
  for (const { summary } of runs) for (const row of summary.models) {
    const key = `${row.model}\0${row.digest}`;
    if (!models.has(key)) models.set(key, structuredClone(row));
    else if (row.planned) {
      const prior = models.get(key);
      if (!prior.planned) models.set(key, structuredClone(row));
      else {
        for (const field of ['planned', 'attempted', 'passed', 'failed', 'errors', 'not_attempted']) prior[field] += row[field];
        for (const metric of ['tool_choice', 'arguments', 'followup']) for (const field of ['passed', 'total']) prior[metric][field] += row[metric][field];
        prior.status = prior.not_attempted ? 'incomplete' : 'measured-smoke';
      }
    }
  }
  return `${renderTable({ models: [...models.values()] })}\n`;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dirs = []; let out;
  for (let i = 2; i < process.argv.length; i++) {
    const flag = process.argv[i], value = process.argv[++i];
    if (!value || !['--run', '--out'].includes(flag)) throw new Error('Usage: node scripts/reliability/tool-report.mjs --run RUN_DIR [--run RUN_DIR] --out NEW_TABLE.md');
    if (flag === '--run') dirs.push(value); else out = value;
  }
  if (!out) throw new Error('Missing --out');
  await writeFile(out, await combinedTable(dirs), { flag: 'wx' });
}
