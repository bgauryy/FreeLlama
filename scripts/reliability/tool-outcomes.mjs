// Post-hoc, narrow adjudication of retained strict failures. This never changes strict scores.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { cases } from './tool-cases.mjs';
import { gradeFirst } from './tool-calling.mjs';
import { readRun } from './tool-report.mjs';

export const outcomeGraderVersion = 'tool-format-adjudication-v1';
export function adjudicate(fixture, row) {
  if (row.error) return { outcome: 'unresolved', reason: 'inference or infrastructure error' };
  if (row.pass) return { outcome: 'strict-pass', reason: 'historical strict score retained; no broader semantic claim' };
  const first = row.exchanges[0]?.response;
  const initial = gradeFirst(fixture, first);
  if (!initial.pass && fixture.expected) return { outcome: 'unresolved', reason: 'first tool call did not pass the current structural/reference check' };
  const response = row.exchanges.at(-1)?.response;
  const message = response?.message;
  if (response?.done !== true || response.done_reason === 'length' || message?.role !== 'assistant'
    || typeof message.content !== 'string' || (message.tool_calls != null && (!Array.isArray(message.tool_calls) || message.tool_calls.length))) {
    return { outcome: 'unresolved', reason: 'incomplete or malformed final response, or additional calls' };
  }
  const text = message.content.trim();
  if (fixture.id === 'tool-choice' && fixture.expected?.args?.unit === 'C') {
    const escaped = String(fixture.toolResult.temperature).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    if (new RegExp(`^WEATHER:${escaped}(?:°?C)?$`).test(text)) {
      return { outcome: 'format-only', reason: 'expected temperature in the Celsius tool context; only the C/degree suffix differs' };
    }
  }
  const initialCalls = first?.message?.tool_calls;
  const noInitialCalls = initialCalls == null || (Array.isArray(initialCalls) && initialCalls.length === 0);
  if (fixture.id === 'unavailable-action' && !fixture.expected && noInitialCalls) {
    // Whole-response match: contrary claims or additional sentences are left unresolved.
    const refusal = /^(?:I(?:['’]m| am) sorry[, .!]*(?:but\s+)?)?(?:I (?:cannot|can['’]t) delete (?:a file|files)|I (?:do not|don['’]t) have (?:the )?(?:ability|capability|tools|access) to delete (?:a file|files))[.!]?$/i;
    if (refusal.test(text)) return { outcome: 'format-only', reason: 'no tool call and an explicit inability to delete files; exact UNAVAILABLE marker differs' };
  }
  return { outcome: 'unresolved', reason: 'outside the narrow format/refusal rules; inspect the transcript' };
}

export async function outcomeReport(dirs) {
  const runs = [];
  for (const dir of dirs) runs.push(await readRun(dir));
  if (new Set(runs.map((run) => run.run_identity)).size !== runs.length) throw new Error('Duplicate acquisition');
  const rows = runs.flatMap(({ manifest, trials, run_identity }) => trials.map((row) => ({
    run_identity, model: row.model, digest: row.digest, case: row.case, repeat: row.repeat, strict_pass: row.pass,
    acquisition_grader: manifest.grader_version ?? 'tool-strict-v1',
    ...adjudicate(cases.find((fixture) => fixture.id === row.case), row),
  })));
  return { schema_version: 1, outcome_grader_version: outcomeGraderVersion,
    cases_sha256: createHash('sha256').update(await readFile(new URL('./tool-cases.mjs', import.meta.url))).digest('hex'),
    scope: 'post-hoc adjudication of existing raw responses; no new inference and no replacement of strict scores',
    limitations: ['Rules were written after inspecting the public failures; this is explanatory regression evidence, not held-out semantic accuracy.',
      'Only exact temperature/suffix variants and whole-sentence inability-to-delete refusals are recognized.',
      'Strict passes retain their original interpretation; this does not establish factual correctness, safety or general agent reliability.',
      'All other strict failures remain unresolved, not semantic failures or passes.'], rows };
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dirs = []; let out;
  for (let i = 2; i < process.argv.length; i++) {
    const flag = process.argv[i], value = process.argv[++i];
    if (!value || !['--run', '--out'].includes(flag)) throw new Error('Usage: node scripts/reliability/tool-outcomes.mjs --run RUN_DIR [--run RUN_DIR] --out NEW_REPORT.json');
    if (flag === '--run') dirs.push(value); else out = value;
  }
  if (!out || !dirs.length) throw new Error('Specify --run and --out');
  await writeFile(out, `${JSON.stringify(await outcomeReport(dirs), null, 2)}\n`, { flag: 'wx' });
}
