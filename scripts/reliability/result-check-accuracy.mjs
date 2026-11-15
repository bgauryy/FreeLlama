import { build } from 'esbuild';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { fixtures, suiteVersion } from './check-fixtures.mjs';

export function confusion(rows) {
  const counts = { tp: 0, fp: 0, tn: 0, fn: 0 };
  for (const row of rows) counts[row.risk ? row.warning ? 'tp' : 'fn' : row.warning ? 'fp' : 'tn']++;
  const { tp, fp, fn } = counts;
  return { ...counts, precision: tp + fp ? tp / (tp + fp) : null, recall: tp + fn ? tp / (tp + fn) : null };
}

export async function evaluateChecks() {
  const dir = await mkdtemp(path.join(tmpdir(), 'freellama-check-eval-'));
  try {
    const entry = fileURLToPath(new URL('../../packages/mcp/src/result-checks.ts', import.meta.url));
    const compiled = await build({ entryPoints: [entry], bundle: true, write: false, format: 'esm', platform: 'node', target: 'node22' });
    const api = await import(`data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`);
    const rows = [];
    for (const fixture of fixtures) {
      let checks;
      if (fixture.family === 'input-instruction-risk') checks = [api.injectionCheck(fixture.input)].filter(Boolean);
      else if (fixture.family === 'null-extraction-risk') checks = [api.nullFieldCheck(fixture.answer, fixture.input)].filter(Boolean);
      else if (fixture.family === 'tool-structural-risk') {
        const calls = [{ function: { name: 'lookup', arguments: fixture.args } }];
        checks = api.resultChecks({ answer: '', structured: { response: { message: { tool_calls: calls } } },
          tools: [{ type: 'function', function: { name: 'lookup', parameters: fixture.parameters } }] });
      } else {
        const file = path.join(dir, `${fixture.id}.txt`);
        await writeFile(file, fixture.source);
        checks = api.resultChecks({ answer: fixture.answer, structured: { files_read: [{ path: file }] } });
      }
      const warning = checks.some((check) => fixture.family === 'source-factual-risk' ? check.startsWith('not in the source:')
        : fixture.family === 'tool-structural-risk' ? !/^\d+ tool call\(s\) match/.test(check) : true);
      const { detected: baseline_warning, ...labeled } = fixture;
      rows.push({ ...labeled, baseline_warning, warning, checks });
    }
    return { schema_version: 2, suite_version: suiteVersion, checker_revision: 'numeric-tokens-v2', baseline_report: 'result-check-accuracy.json', evidence: 'synthetic labeled regression fixtures; no model inference',
      limitations: ['Labels target review-worthy risk, not arbitrary nonempty check lines.', 'Curated cases are regression after fixes, not held-out production accuracy or a model ranking.',
        'Lexical source presence does not verify facts, relationships, calculations or completeness.', 'Tool checks cover a documented schema subset; unsupported keywords are warnings, not validation.'],
      families: Object.fromEntries([...new Set(rows.map((row) => row.family))].map((family) => [family, confusion(rows.filter((row) => row.family === family))])), rows };
  } finally { await rm(dir, { recursive: true, force: true }); }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  if (args.length !== 2 || args[0] !== '--out') throw new Error('Usage: node scripts/reliability/result-check-accuracy.mjs --out REPORT.json');
  await writeFile(args[1], `${JSON.stringify(await evaluateChecks(), null, 2)}\n`, { flag: 'wx' });
  console.log(`Wrote ${args[1]}`);
}
