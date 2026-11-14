import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { dashboardData, pooledData as dashboardPooled, renderDashboard, renderPooledReport } from './report.mjs';

const measured = (id, value = 20) => ({ id, status: 'complete', attempted: 4, success: 3, quality_pass_rate: 0.75, verified_elapsed_ms: { n: 3, p50: value, p95: value * 2 }, measured_window_ms: 4000, useful_tasks_per_minute: 45, local_tokens: { combined_coverage: 1, combined_known_attempts: 4, total_per_verified_completion: 100 }, host_observations: { sample_count: 5, min_available_bytes: 8 * 1024 ** 3, cpu_busy_percent: { time_weighted_mean: 25, covered_ms: 4000, coverage: 1 } } });

test('default report has exactly seven systems and missing data never becomes numeric zero', () => {
  const data = dashboardData({ vendors: [{ id: 'ollama', status: 'unavailable', attempted: 0, success: 0, quality_pass_rate: 0 }] });
  assert.deepEqual(data.systems.map(s => s.name), ['Ollama alone', 'FreeLlama', 'FreeLlama (streamed)', 'llama.cpp', 'llama-swap', 'LocalAI', 'oMLX']);
  for (const system of data.systems) for (const metric of Object.values(system.metrics)) assert.equal(metric.value, null);
  const html = renderDashboard({ vendors: [] });
  assert.equal((html.match(/class="unknown"/g) ?? []).length, 42);
  assert.equal((html.match(/aria-label="Not measured"/g) ?? []).length, 42);
  assert.doesNotMatch(html, /<circle/);
  assert.match(html, /No measured vendor comparison/);
});

test('real measurements use their own denominators and exclude negative or incomplete metrics', () => {
  const row = measured('freellama');
  const data = dashboardData({ vendors: [row] });
  assert.deepEqual(Object.fromEntries(Object.entries(data.systems[1].metrics).map(([key, metric]) => [key, metric.value])), { quality: 75, task_time: 20, throughput: 45, local_tokens: 100, host_cpu: 25, memory: 8 });
  row.quality_pass_rate = -1; row.verified_elapsed_ms.p50 = -20; row.useful_tasks_per_minute = -1;
  row.local_tokens.combined_coverage = 0.75; row.host_observations.cpu_busy_percent.time_weighted_mean = 101; row.host_observations.min_available_bytes = -1;
  for (const metric of Object.values(dashboardData({ vendors: [row] }).systems[1].metrics)) assert.equal(metric.value, null);
});

test('task-time p95 is secondary to its measured median and missing p95 stays unavailable', () => {
  const row = measured('freellama');
  row.protocol = 'freellama-managed';
  row.verified_first_content_ms = { n: 0, p50: null, p95: null };
  let html = renderDashboard({ vendors: [row] });
  assert.match(html, />20<small>p95 40 ms · n=3<\/small>/);
  assert.match(html, /managed-task TTFT stays unavailable/);
  delete row.verified_elapsed_ms.p95;
  html = renderDashboard({ vendors: [row] });
  assert.match(html, />20<small>p95 N\/A · n=3<\/small>/);
});

test('unqualified or single-system values cannot produce comparison plots', () => {
  const report = { vendors: [measured('ollama'), measured('freellama', 30)] };
  assert.doesNotMatch(renderDashboard(report), /<circle/);
  report.comparability = { verified: true, cohort_id: 'same model and task' };
  assert.match(renderDashboard(report), /<circle/);
  report.vendors.pop();
  assert.doesNotMatch(renderDashboard(report), /<circle/);
});

test('qualified cohorts and metric exceptions control plots without inserting missing systems', () => {
  const first = { ...measured('ollama'), comparison_qualified: true, comparison_cohort: 'A' };
  const second = { ...measured('freellama'), comparison_qualified: true, comparison_cohort: 'B' };
  const report = { comparability: { metrics: { memory: false } }, vendors: [first, second] };
  assert.equal(dashboardData(report).comparisons.task_time.length, 0);
  second.comparison_cohort = 'A';
  const data = dashboardData(report);
  assert.equal(data.comparisons.task_time[0].points.length, 2);
  assert.equal(data.comparisons.memory.length, 0);
  assert.deepEqual(data.comparisons.task_time[0].points.map(p => p.name), ['Ollama alone', 'FreeLlama']);
  report.comparability.verified = true;
  second.comparison_cohort = 'B';
  assert.equal(dashboardData(report).comparisons.task_time.length, 0);
});

test('explicitly measured zero remains distinct from unavailable observations', () => {
  const row = measured('ollama'); row.success = 0; row.quality_pass_rate = 0; row.useful_tasks_per_minute = 0;
  const data = dashboardData({ vendors: [row] });
  assert.equal(data.systems[0].metrics.quality.value, 0);
  assert.equal(data.systems[0].metrics.throughput.value, 0);
  assert.equal(data.systems[0].metrics.local_tokens.value, null);
  assert.equal(data.systems[1].metrics.throughput.value, null);
});

test('native samples stay separate and recompute spread instead of trusting reported deviation', () => {
  const sanity = { rows: [{ n_prompt: 128, n_gen: 0, samples_ts: [32, 64, -1], samples_ns: [4e9, 2e9, -1], avg_ts: 999, stddev_ns: 999 }], resources: { wall_seconds: 8, cpu_core_equivalents: 2, peak_rss_bytes: 1024 ** 3, scope: 'Native process only' } };
  const data = dashboardData({ vendors: [] }, { sanity });
  assert.equal(data.sanity.rows[0].mean, 48);
  assert.equal(data.sanity.rows[0].n, 2);
  assert.equal(data.sanity.rows[0].excluded, 1);
  assert.equal(data.sanity.rows[0].pooled, 256 / 6);
  assert.equal(data.systems[2].metrics.throughput.value, null);
  const html = renderDashboard({ vendors: [] }, { sanity });
  assert.match(html, /Separate native sanity run/);
  assert.match(html, /Native process only/);
  assert.match(html, /1,024 MiB/);
  assert.equal((html.match(/<circle/g) ?? []).length, 2);
  sanity.rows[0].samples_ns = [4e9, -1, 2e9];
  assert.equal(dashboardData({}, { sanity }).sanity.rows[0].pooled, null);
});

test('HTML and embedded JSON escape hostile text and do not request external assets', () => {
  const payload = '</script><img src=x onerror="alert(1)">\u2028&';
  const html = renderDashboard({ title: payload, vendors: [{ id: 'ollama', status: payload, error: payload }], sources: { raw: payload } }, { sanity: { scope: payload, rows: [] } });
  assert.doesNotMatch(html, /<img|onerror="alert/);
  assert.match(html, /\\u003c\/script\\u003e/);
  assert.doesNotMatch(html, /<script[^>]+src=|<link|fetch\(|XMLHttpRequest|https?:\/\//);
  const json = html.match(/<script id="benchmark-data" type="application\/json">([\s\S]*?)<\/script>/)[1];
  assert.equal(JSON.parse(json).title, payload);
});

test('native host CPU is labelled separately from process CPU and unavailable RSS', () => {
  const sanity = {
    outcome: { code: 0 }, rows: [{ n_prompt: 128, n_gen: 0, samples_ts: [0, 32], samples_ns: [1e9, 4e9] }],
    resources: { wall_seconds: 16.45, cpu_core_equivalents: 3.6766, peak_rss_bytes: null, scope: 'Native process only' },
    host_observations: { cpu_busy_percent: { time_weighted_mean: 56.5272, peak: 72.1572, covered_ms: 16450, coverage: 1, scope: 'Entire host during native run, including other applications' } },
  };
  const data = dashboardData({}, { sanity });
  assert.equal(data.sanity.rows[0].n, 1);
  assert.equal(data.sanity.rows[0].excluded, 1);
  assert.equal(data.sanity.rows[0].mean, 32);
  assert.equal(data.sanity.rows[0].pooled, 32);
  const html = renderDashboard({}, { sanity });
  assert.match(html, /Process CPU<\/dt><dd>3\.68 core equivalents/);
  assert.match(html, /Peak process RSS<\/dt><dd>Not measured/);
  assert.match(html, /Whole-host CPU mean<\/dt><dd>56\.53%/);
  assert.match(html, /Whole-host CPU peak<\/dt><dd>72\.16%/);
  assert.match(html, /Host CPU counter coverage<\/dt><dd>100%/);
  assert.match(html, /Entire host during native run, including other applications/);
});

test('CLI writes a standalone artifact and refuses to overwrite earlier evidence', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'freellama-report-'));
  try {
    const report = path.join(dir, 'report.json'), out = path.join(dir, 'report.html');
    await writeFile(report, JSON.stringify({ vendors: [] }));
    const script = fileURLToPath(new URL('./report.mjs', import.meta.url));
    execFileSync(process.execPath, [script, '--report', report, '--out', out], { stdio: 'pipe' });
    const html = await readFile(out, 'utf8');
    assert.match(html, /^<!doctype html>/);
    assert.throws(() => execFileSync(process.execPath, [script, '--report', report, '--out', out], { stdio: 'pipe' }));
    assert.equal(await readFile(out, 'utf8'), html);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('a successful process exit cannot override failed native validation or cancellation', () => {
  for (const failure of [{ status: 'failed_sanity' }, { timed_out: true }, { interrupted: true }, { parse_error: 'Malformed output' }]) {
    const sanity = { status: 'complete_sanity', outcome: { code: 0 }, ...failure };
    assert.equal(dashboardData({}, { sanity }).sanity.status, 'Failed or partial');
  }
  assert.equal(dashboardData({}, { sanity: { outcome: { code: 0 } } }).sanity.status, 'Process exited successfully; validation not supplied');
  assert.equal(dashboardData({}, { sanity: { status: 'complete_sanity', outcome: { code: 0 } } }).sanity.status, 'Complete');
});

test('pooled report takes p50/p95 from raw verified trials across orders and keeps per-order tables', () => {
  const trials = (values, extra = []) => [{ warmup: true, outcome: 'verified', elapsed_ms: 1 }, ...values.map(elapsed_ms => ({ warmup: false, outcome: 'verified', elapsed_ms, first_content_ms: elapsed_ms / 2 })), ...extra];
  const entries = [
    { label: 'forward', report: { vendors: [measured('ollama', 10), measured('freellama', 30)] }, trials: { ollama: trials([10, 20, 30]), freellama: trials([30], [{ warmup: false, outcome: 'failed', elapsed_ms: 999 }]) } },
    { label: 'reversed', report: { vendors: [measured('ollama', 40), { ...measured('freellama', 50), status: 'failed' }] }, trials: { ollama: trials([40, 50]), freellama: null } },
  ];
  const pooled = dashboardPooled(entries);
  const ollama = pooled.find(system => system.id === 'ollama');
  assert.deepEqual([ollama.orders_pooled, ollama.attempted, ollama.verified, ollama.quality], [2, 5, 5, 100]);
  assert.deepEqual(ollama.task_time, { n: 5, p50: 30, p95: 50 });
  assert.deepEqual(ollama.ttft, { n: 5, p50: 15, p95: 25 });
  const freellama = pooled.find(system => system.id === 'freellama');
  assert.deepEqual([freellama.orders_pooled, freellama.attempted, freellama.verified, freellama.quality], [1, 2, 1, 50]);
  assert.equal(freellama.orders[1].raw_trials, null);
  assert.equal(pooled.find(system => system.id === 'omlx').task_time.p50, null);
  const html = renderPooledReport(entries);
  assert.match(html, /Pooled across orders/);
  assert.match(html, /Order 1: forward/);
  assert.match(html, /Order 2: reversed/);
  assert.match(html, /reversed: Failed · no raw trials/);
  assert.equal((html.match(/<table>/g) ?? []).length, 3);
  assert.doesNotMatch(html, /<script[^>]+src=|https?:\/\//);
});

test('CLI: one --report is unchanged; repeated --report reads trials.jsonl next to each summary', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'freellama-report-pooled-'));
  try {
    const script = fileURLToPath(new URL('./report.mjs', import.meta.url));
    const runs = [];
    for (const [name, values] of [['forward', [10, 20]], ['reversed', [30, 40]]]) {
      await mkdir(path.join(dir, name, 'ollama'), { recursive: true });
      const summary = { vendors: [measured('ollama')] };
      await writeFile(path.join(dir, name, 'summary.json'), JSON.stringify(summary));
      await writeFile(path.join(dir, name, 'ollama', 'trials.jsonl'), values.map(elapsed_ms => JSON.stringify({ warmup: false, outcome: 'verified', elapsed_ms })).join('\n') + '\n');
      runs.push(path.join(dir, name, 'summary.json'));
    }
    const single = path.join(dir, 'single.html');
    execFileSync(process.execPath, [script, '--report', runs[0], '--out', single], { stdio: 'pipe' });
    assert.equal(await readFile(single, 'utf8'), renderDashboard(JSON.parse(await readFile(runs[0], 'utf8'))));
    const out = path.join(dir, 'pooled.html');
    execFileSync(process.execPath, [script, '--report', runs[0], '--report', runs[1], '--out', out], { stdio: 'pipe' });
    const data = JSON.parse((await readFile(out, 'utf8')).match(/<script id="benchmark-data" type="application\/json">([\s\S]*?)<\/script>/)[1]);
    assert.deepEqual(data.orders, ['forward', 'reversed']);
    assert.deepEqual(data.pooled.find(system => system.id === 'ollama').task_time, { n: 4, p50: 20, p95: 40 });
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('macOS estimated memory is labelled in the cell note, not as kernel memory', () => {
  const row = measured('ollama');
  row.host_observations.available_bytes_source = 'freellama_estimate';
  assert.match(dashboardData({ vendors: [row] }).systems[0].metrics.memory.note, /FreeLlama estimate/);
  assert.equal(dashboardData({ vendors: [measured('ollama')] }).systems[0].metrics.memory.note, '');
});
