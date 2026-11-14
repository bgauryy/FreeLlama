#!/usr/bin/env node
// Standalone presentation of recorded evidence. No requests, model calls or inferred scores.
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const DEFAULT_SYSTEMS = [
  { id: 'ollama', name: 'Ollama alone' },
  { id: 'freellama', name: 'FreeLlama' },
  { id: 'freellama-stream', name: 'FreeLlama (streamed)' },
  { id: 'llama-cpp', name: 'llama.cpp' },
  { id: 'llama-swap', name: 'llama-swap' },
  { id: 'localai', name: 'LocalAI' },
  { id: 'omlx', name: 'oMLX' },
];
const METRICS = [
  { id: 'quality', name: 'Quality', unit: '% correct', detail: 'Exact-case pass rate; all measured attempts remain in the denominator.' },
  { id: 'task_time', name: 'Task time', unit: 'ms · median', detail: 'Client wall time for verified answers. Failures remain in outcome counts.' },
  { id: 'throughput', name: 'Throughput', unit: 'verified tasks/min', detail: 'Verified completions divided by the entire measured request window.' },
  { id: 'local_tokens', name: 'Local tokens', unit: 'tokens / verified task', detail: 'All measured local prompt and output tokens, including failed effort. Requires complete token coverage. Paid-agent cost is separate.' },
  { id: 'host_cpu', name: 'Host CPU', unit: '% busy · mean', detail: 'Whole-host CPU capacity from cumulative OS counters; includes other applications.' },
  { id: 'memory', name: 'Memory', unit: 'GiB available · min', detail: 'Lowest reported available host memory. This is not process RSS; missing counters remain unknown.' },
];
const STYLE = ':root{color-scheme:light;--ink:#16242d;--muted:#53636d;--line:#d7dfe4;--accent:#15657b;--paper:#fff;--wash:#f3f6f8}*{box-sizing:border-box}body{margin:0;background:var(--wash);color:var(--ink);font:15px/1.5 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}main{max-width:1200px;margin:0 auto;padding:32px 28px 48px}header{padding-bottom:20px;border-bottom:2px solid var(--ink)}h1{font-size:30px;letter-spacing:-.03em;margin:0 0 10px}h2{font-size:20px;letter-spacing:-.02em;margin:0 0 8px}p{margin:8px 0}.subhead,.note,.muted,small{color:var(--muted)}.verdict{font-size:17px;font-weight:600}section{margin-top:28px}.table-scroll{overflow-x:auto;background:var(--paper);border:1px solid var(--line);margin-top:16px}table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}th,td{padding:14px 12px;text-align:left;border-bottom:1px solid var(--line);vertical-align:top}thead th{background:#edf2f5;font-size:13px;font-weight:600;white-space:nowrap}thead small{font-weight:400}tbody tr:last-child>*{border-bottom:0}tbody th{min-width:180px;font-weight:600}td{min-width:120px}.unknown{color:var(--muted);font-size:13px}small{display:block;font-size:12px;font-weight:400;margin-top:4px}.row-note{max-width:250px;line-height:1.35}.measured{font-weight:600}.note{font-size:13px;max-width:100ch}.controls{display:flex;align-items:center;gap:12px;margin:14px 0}select{font:inherit;color:var(--ink);background:var(--paper);border:1px solid #8b9ca8;border-radius:4px;padding:7px 10px}select:focus-visible{outline:3px solid #9bd2e1;outline-offset:2px}.empty{padding:16px;background:#edf2f5;color:var(--muted);border-left:3px solid #9aaab5}.plot{margin:14px 0;max-width:880px}.plot figcaption{font-size:13px;color:var(--muted);margin-bottom:8px}.plot svg{display:block;width:100%;max-width:760px;background:var(--paper);border:1px solid var(--line);padding:10px}.plot text,.samples text{font:12px system-ui,sans-serif;fill:var(--ink)}.plot text.muted,.samples text.muted{fill:var(--muted)}.plot .plot-name{font-weight:600}.plot .plot-value{font-variant-numeric:tabular-nums}.track{stroke:#d1dbe1;stroke-width:2}.point{fill:var(--accent);stroke:var(--paper);stroke-width:1}.samples{width:300px;height:58px;display:block}.sanity-table th,.sanity-table td{vertical-align:middle}.resource-line{display:flex;gap:36px;flex-wrap:wrap;border-top:1px solid var(--line);padding-top:16px;margin-top:20px}.resource-line dt{font-size:12px;color:var(--muted)}.resource-line dd{margin:3px 0 0;font-weight:600}details{margin-top:24px;color:var(--muted);font-size:13px}summary{cursor:pointer}details li{overflow-wrap:anywhere;margin:6px 0}footer{margin-top:28px;padding-top:16px;border-top:1px solid var(--line);font-size:12px;color:var(--muted)}[hidden]{display:none!important}@media(max-width:640px){main{padding:22px 16px}h1{font-size:25px}th,td{padding:11px 10px}.resource-line{gap:18px}body{font-size:14px}}';
const number = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
const positive = value => number(value) !== null && value > 0 ? value : null;
const integer = value => Number.isSafeInteger(value) && value >= 0 ? value : null;
const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const safeJson = value => JSON.stringify(value).replace(/[<>&\u2028\u2029]/g, c => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
const format = value => value === null ? 'Not measured' : new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 }).format(value);
const mean = values => values.reduce((sum, value) => sum + value, 0) / values.length;
const sampleSd = values => values.length > 1 ? Math.sqrt(values.reduce((sum, value) => sum + (value - mean(values)) ** 2, 0) / (values.length - 1)) : null;
const cellMetric = (value, n, note = '') => ({ value: number(value), n: integer(n), note });

function matchVendor(spec, vendors) {
  const aliases = spec.id === 'llama-cpp' ? ['llama-cpp', 'llama.cpp'] : spec.id === 'freellama' ? ['freellama', 'freellama-managed'] : [spec.id];
  return vendors.find(v => aliases.includes(v.id) && v.protocol !== 'freellama') ?? vendors.find(v => aliases.includes(v.id));
}
const resolveSpec = spec => typeof spec === 'string' ? DEFAULT_SYSTEMS.find(s => s.id === spec) ?? { id: spec, name: spec } : spec;

function normalizeSystem(spec, vendors) {
  const row = matchVendor(spec, vendors);
  const attempts = integer(row?.attempted) ?? 0;
  const cpu = row?.host_observations?.cpu_busy_percent;
  const token = row?.local_tokens;
  const quality = number(row?.quality_pass_rate);
  const memory = number(row?.host_observations?.min_available_bytes);
  const timeN = integer(row?.verified_elapsed_ms?.n);
  const cpuValue = number(cpu?.time_weighted_mean);
  return {
    id: spec.id, name: spec.name, status: row?.status ?? 'not_run', note: row?.error ?? row?.notes ?? '',
    attempted: attempts, verified: integer(row?.success), comparison_cohort: row?.comparison_cohort ?? null,
    comparison_qualified: row?.comparison_qualified === true,
    metrics: {
      quality: cellMetric(attempts > 0 && quality !== null && quality <= 1 ? quality * 100 : null, attempts),
      task_time: { ...cellMetric(timeN > 0 ? row?.verified_elapsed_ms?.p50 : null, timeN), p95: timeN > 0 ? number(row?.verified_elapsed_ms?.p95) : null },
      throughput: cellMetric(attempts > 0 && positive(row?.measured_window_ms) !== null ? row?.useful_tasks_per_minute : null, attempts),
      local_tokens: cellMetric(token?.combined_coverage === 1 && positive(row?.success) !== null ? token.total_per_verified_completion : null, token?.combined_known_attempts, token?.combined_coverage === 1 ? '' : 'Incomplete or absent token coverage'),
      host_cpu: cellMetric(positive(cpu?.covered_ms) !== null && cpuValue !== null && cpuValue <= 100 ? cpuValue : null, row?.host_observations?.sample_count, number(cpu?.coverage) === null ? '' : `Coverage ${format(cpu.coverage * 100)}%`),
      memory: cellMetric(positive(row?.host_observations?.sample_count) !== null && memory !== null ? memory / 1024 ** 3 : null, row?.host_observations?.sample_count, row?.host_observations?.available_bytes_source === 'freellama_estimate' ? 'macOS FreeLlama estimate (free + bounded reclaimable), not a kernel value' : ''),
    },
  };
}

function comparisonGroups(systems, comparability, metric) {
  if (comparability?.metrics?.[metric] === false) return [];
  const groups = new Map();
  for (const system of systems) {
    if (system.metrics[metric].value === null) continue;
    const cohort = comparability?.verified === true ? system.comparison_cohort ?? comparability.cohort_id ?? 'Matched cohort' : system.comparison_qualified ? system.comparison_cohort : null;
    if (typeof cohort !== 'string' || !cohort) continue;
    const points = groups.get(cohort) ?? [];
    points.push({ name: system.name, ...system.metrics[metric] }); groups.set(cohort, points);
  }
  return [...groups].filter(([, points]) => points.length >= 2).map(([name, points]) => ({ name, points }));
}

function normalizeSanity(sanity) {
  if (!sanity || typeof sanity !== 'object') return null;
  const rows = (Array.isArray(sanity.rows) ? sanity.rows : []).map(row => {
    const speeds = Array.isArray(row.samples_ts) ? row.samples_ts : [];
    const validIndices = speeds.map((value, index) => positive(value) !== null ? index : null).filter(index => index !== null);
    const values = validIndices.map(index => speeds[index]);
    const prompt = positive(row.n_prompt), generated = positive(row.n_gen);
    if (!values.length || (prompt === null && generated === null)) return null;
    const kind = prompt !== null && generated !== null ? `Prompt ${prompt} + generate ${generated}` : prompt !== null ? `Prefill ${prompt}` : `Generate ${generated}`;
    const times = Array.isArray(row.samples_ns) && row.samples_ns.length === speeds.length ? validIndices.map(index => row.samples_ns[index]) : [];
    const tokens = (prompt ?? 0) + (generated ?? 0);
    const pooled = times.length === values.length && times.every(value => positive(value) !== null) ? tokens * times.length / (times.reduce((sum, value) => sum + value, 0) / 1e9) : null;
    return { label: kind, n: values.length, values, mean: mean(values), sd: sampleSd(values), pooled, excluded: (row.samples_ts?.length ?? 0) - values.length };
  }).filter(Boolean);
  const resources = sanity.resources ?? {};
  const hostCpu = sanity.host_observations?.cpu_busy_percent ?? {};
  const hostMean = number(hostCpu.time_weighted_mean), hostPeak = number(hostCpu.peak), coverage = number(hostCpu.coverage);
  const failed = sanity.status === 'failed_sanity' || sanity.timed_out === true || sanity.interrupted === true
    || Boolean(sanity.parse_error || sanity.outcome?.error)
    || (sanity.outcome?.code !== undefined && sanity.outcome.code !== 0);
  const status = failed ? 'Failed or partial'
    : sanity.status === 'complete_sanity' && sanity.outcome?.code === 0 ? 'Complete'
    : sanity.outcome?.code === 0 ? 'Process exited successfully; validation not supplied'
    : sanity.status ?? 'Outcome not supplied';
  return {
    title: sanity.title ?? 'Native CPU sanity check', date: sanity.finished_at ?? sanity.started_at ?? '',
    model: sanity.rows?.[0]?.model_type ?? sanity.model ?? '', scope: sanity.scope ?? 'Synthetic native engine measurement, outside the common task cohort.',
    status,
    rows, resources: { wall_seconds: number(resources.wall_seconds), user_seconds: number(resources.user_seconds), system_seconds: number(resources.system_seconds), cpu_core_equivalents: number(resources.cpu_core_equivalents), peak_rss_bytes: number(resources.peak_rss_bytes), scope: resources.scope ?? 'Process resource scope was not supplied.' },
    host_cpu: { mean: positive(hostCpu.covered_ms) !== null && hostMean !== null && hostMean <= 100 ? hostMean : null, peak: positive(hostCpu.covered_ms) !== null && hostPeak !== null && hostPeak <= 100 ? hostPeak : null, coverage: coverage !== null && coverage <= 1 ? coverage : null, scope: hostCpu.scope ?? 'Whole-host CPU includes the native run and unrelated applications; it is separate from measured process CPU.' },
  };
}

export function dashboardData(report = {}, { systems = DEFAULT_SYSTEMS, sanity } = {}) {
  const vendors = Array.isArray(report.vendors) ? report.vendors : [];
  const selected = systems.map(spec => normalizeSystem(resolveSpec(spec), vendors));
  const comparisons = Object.fromEntries(METRICS.map(metric => [metric.id, comparisonGroups(selected, report.comparability, metric.id)]));
  return { title: report.title ?? 'FreeLlama benchmark', date: report.finished_at ?? report.started_at ?? report.date ?? '', scope: report.scope ?? 'Common task comparison', comparability_reason: typeof report.comparability?.reason === 'string' ? report.comparability.reason : '', systems: selected, comparisons, sanity: normalizeSanity(sanity), sources: report.sources && typeof report.sources === 'object' ? Object.fromEntries(Object.entries(report.sources).filter(([, value]) => typeof value === 'string')) : {} };
}

function comparisonPlot(group, metric, index) {
  const max = Math.max(...group.points.map(point => point.value), 1);
  const height = group.points.length * 42 + 42;
  const title = `plot-${metric.id}-${index}`;
  return `<figure class="plot"><figcaption>${esc(group.name)} · ${esc(metric.unit)}</figcaption><svg viewBox="0 0 720 ${height}" role="img" aria-labelledby="${title}"><title id="${title}">${esc(metric.name)} for measured systems in ${esc(group.name)}. Missing values are excluded.</title>${group.points.map((point, i) => {
    const y = i * 42 + 24, x = 192 + point.value / max * 320;
    return `<text x="4" y="${y + 4}" class="plot-name">${esc(point.name)}</text><line x1="192" x2="512" y1="${y}" y2="${y}" class="track"/><circle cx="${x}" cy="${y}" r="5" class="point"/><text x="534" y="${y + 4}" class="plot-value">${esc(format(point.value))}</text><text x="644" y="${y + 4}" class="muted">n=${esc(point.n ?? '?')}</text>`;
  }).join('')}<text x="192" y="${height - 4}" class="muted">0</text><text x="512" y="${height - 4}" class="muted" text-anchor="end">${esc(format(max))}</text></svg></figure>`;
}

function sanityPlot(row, index) {
  const min = Math.min(...row.values), max = Math.max(...row.values), span = Math.max(max - min, 1);
  const title = `sanity-plot-${index}`;
  return `<svg class="samples" viewBox="0 0 360 58" role="img" aria-labelledby="${title}"><title id="${title}">${esc(row.label)}: ${row.n} native repetition speeds in tokens per second; range ${esc(format(min))} to ${esc(format(max))}.</title><line x1="20" x2="340" y1="20" y2="20" class="track"/>${row.values.map((value, i) => `<circle cx="${20 + (value - min) / span * 320}" cy="${17 + i % 3 * 3}" r="4" class="point"><title>Repetition ${i + 1}: ${esc(format(value))} tokens/s</title></circle>`).join('')}<text x="20" y="49" class="muted">${esc(format(min))}</text><text x="340" y="49" text-anchor="end" class="muted">${esc(format(max))} tokens/s</text></svg>`;
}

const statusLabel = status => ({ complete: 'Complete', unavailable: 'Blocked', blocked: 'Blocked', failed: 'Failed', not_run: 'Not run' }[status] ?? String(status));
function commonTable(data) {
  const commonRows = data.systems.map(system => `<tr><th scope="row">${esc(system.name)}<small>${esc(statusLabel(system.status))} · n=${system.attempted}${system.verified === null ? '' : ` · ${system.verified} verified`}</small>${system.comparison_cohort ? `<small>Cohort: ${esc(system.comparison_cohort)} · declared</small>` : ''}${system.note ? `<small class="row-note">${esc(system.note)}</small>` : ''}</th>${METRICS.map(metric => {
    const value = system.metrics[metric.id];
    const display = value.value === null ? '<span class="missing" role="img" aria-label="Not measured">—</span>' : esc(format(value.value));
    const tail = value.value === null ? '' : metric.id === 'task_time' ? `<small>p95 ${value.p95 === null ? 'N/A' : `${esc(format(value.p95))} ms`}${value.n === null ? '' : ` · n=${value.n}`}</small>` : value.n === null ? '' : `<small>n=${value.n}</small>`;
    return `<td class="${value.value === null ? 'unknown' : 'measured'}" title="${esc(value.note || metric.detail)}">${display}${tail}</td>`;
  }).join('')}</tr>`).join('');
  return `<div class="table-scroll"><table><thead><tr><th scope="col">System / outcome</th>${METRICS.map(metric => `<th scope="col" title="${esc(metric.detail)}">${esc(metric.name)}<small>${esc(metric.unit)}</small></th>`).join('')}</tr></thead><tbody>${commonRows}</tbody></table></div>`;
}

export function renderDashboard(report, options = {}) {
  const data = dashboardData(report, options);
  const hasPlots = Object.values(data.comparisons).some(groups => groups.length);
  const panels = METRICS.map((metric, index) => `<div class="metric-panel" data-metric="${metric.id}"${index ? ' hidden' : ''}>${data.comparisons[metric.id].length ? data.comparisons[metric.id].map((group, i) => comparisonPlot(group, metric, i)).join('') : '<p class="empty">No qualified comparison for this metric. At least two measured systems must share an explicitly verified cohort.</p>'}</div>`).join('');
  const sanitySection = sanityHtml(data.sanity);
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${esc(data.title)}</title><style>
${STYLE}
</style></head><body><main><header><h1>${esc(data.title)}</h1><p class="verdict">${hasPlots ? 'Measured values are shown only within explicitly matched comparison cohorts.' : 'No measured vendor comparison is established yet.'}</p><p class="subhead">${esc(data.date)}${data.date ? ' · ' : ''}${esc(data.scope)}</p></header><section aria-labelledby="common-heading"><h2 id="common-heading">Same task, ${data.systems.length} systems</h2><p class="note">Six common KPIs. — means Not measured; blocked observations are never zeros. Test counts describe the recorded case; they are not confidence scores.</p>${commonTable(data)}<p class="note">Quality and token cost use all measured attempts. Task-time medians and p95 use verified answers; managed-task TTFT stays unavailable. Host CPU/memory cover the full vendor block including warm-ups and unrelated applications; request KPIs exclude warm-ups. Cohort names are declarations and do not verify comparable artifacts. Neither local tokens nor native throughput measures paid-agent cost.</p></section><section aria-labelledby="comparison-heading"><h2 id="comparison-heading">Measured comparisons</h2>${data.comparability_reason ? `<p class="note">${esc(data.comparability_reason)}</p>` : ''}${hasPlots ? `<div class="controls"><label for="metric">Metric</label><select id="metric">${METRICS.map(metric => `<option value="${metric.id}">${esc(metric.name)}</option>`).join('')}</select></div>${panels}` : '<p class="empty">No comparison plot is drawn: two or more measured systems with verified workload/model comparability are required. Missing results are never plotted as zeros.</p>'}</section>${sanitySection}${Object.keys(data.sources).length ? `<details><summary>Recorded source files</summary><ul>${Object.entries(data.sources).map(([name, value]) => `<li>${esc(name)}: <code>${esc(value)}</code></li>`).join('')}</ul></details>` : ''}<footer>Evidence report. No ranking, aggregate score, estimated result, or winner is inferred. This file contains no external requests or dependencies.</footer></main><script id="benchmark-data" type="application/json">${safeJson(data)}</script><script>const metric=document.getElementById('metric');if(metric){metric.addEventListener('change',()=>{for(const panel of document.querySelectorAll('[data-metric]'))panel.hidden=panel.dataset.metric!==metric.value;});}</script></body></html>`;
}

function sanityHtml(sanity) {
  return sanity ? `<section aria-labelledby="sanity-heading"><h2 id="sanity-heading">Separate native sanity run</h2><p>${esc(sanity.status)} · ${esc(sanity.model)}${sanity.date ? ` · ${esc(sanity.date)}` : ''}</p><p class="note">${esc(sanity.scope)} These samples do not populate the common comparison.</p>${sanity.rows.length ? `<div class="table-scroll"><table class="sanity-table"><thead><tr><th scope="col">Synthetic case</th><th scope="col">n</th><th scope="col">Mean ± sample SD<br><small>tokens/s</small></th><th scope="col">Pooled throughput<br><small>tokens/s</small></th><th scope="col">Actual repetition speeds</th></tr></thead><tbody>${sanity.rows.map((row, i) => `<tr><th scope="row">${esc(row.label)}${row.excluded ? `<small>${row.excluded} invalid samples excluded</small>` : ''}</th><td>${row.n}</td><td>${esc(format(row.mean))} ± ${esc(format(row.sd))}</td><td>${esc(format(row.pooled))}</td><td>${sanityPlot(row, i)}</td></tr>`).join('')}</tbody></table></div>` : '<p class="empty">No valid native samples were recorded.</p>'}<dl class="resource-line"><div><dt>Process wall time</dt><dd>${esc(format(sanity.resources.wall_seconds))}${sanity.resources.wall_seconds === null ? '' : ' s'}</dd></div><div><dt>Process CPU</dt><dd>${esc(format(sanity.resources.cpu_core_equivalents))}${sanity.resources.cpu_core_equivalents === null ? '' : ' core equivalents'}</dd></div><div><dt>Peak process RSS</dt><dd>${esc(format(sanity.resources.peak_rss_bytes === null ? null : sanity.resources.peak_rss_bytes / 1024 ** 2))}${sanity.resources.peak_rss_bytes === null ? '' : ' MiB'}</dd></div></dl><p class="note">${esc(sanity.resources.scope)} Native process CPU/RSS and common whole-host CPU/available memory have different scopes.</p><dl class="resource-line host-resources" aria-label="Whole-host CPU during native sanity run"><div><dt>Whole-host CPU mean</dt><dd>${esc(format(sanity.host_cpu.mean))}${sanity.host_cpu.mean === null ? '' : '%'}</dd></div><div><dt>Whole-host CPU peak</dt><dd>${esc(format(sanity.host_cpu.peak))}${sanity.host_cpu.peak === null ? '' : '%'}</dd></div><div><dt>Host CPU counter coverage</dt><dd>${esc(format(sanity.host_cpu.coverage === null ? null : sanity.host_cpu.coverage * 100))}${sanity.host_cpu.coverage === null ? '' : '%'}</dd></div></dl><p class="note">${esc(sanity.host_cpu.scope)}</p></section>` : '';
}

// Same nearest-rank quantiles as lib.mjs distribution().
function quantiles(values) {
  const ordered = values.filter(value => number(value) !== null).sort((a, b) => a - b);
  const at = fraction => ordered.length ? ordered[Math.max(0, Math.ceil(ordered.length * fraction) - 1)] : null;
  return { n: ordered.length, p50: at(0.5), p95: at(0.95) };
}

// Pooled view across run orders. Percentiles come from raw measured trials (warm-ups excluded),
// never from averaging per-order percentiles. An order without raw trials stays out of the pool.
export function pooledData(entries, { systems = DEFAULT_SYSTEMS } = {}) {
  return systems.map(resolveSpec).map(spec => {
    const orders = entries.map(entry => {
      const row = matchVendor(spec, Array.isArray(entry.report?.vendors) ? entry.report.vendors : []);
      const trials = row ? entry.trials?.[row.id] : undefined;
      return { label: entry.label, vendor_id: row?.id ?? null, status: row?.status ?? 'not_run', trials: Array.isArray(trials) ? trials.filter(trial => trial && trial.warmup !== true) : null };
    });
    const measured = orders.flatMap(order => order.trials ?? []);
    const verified = measured.filter(trial => trial.outcome === 'verified');
    return { id: spec.id, name: spec.name, orders: orders.map(({ trials, ...order }) => ({ ...order, raw_trials: trials === null ? null : trials.length })),
      orders_pooled: orders.filter(order => order.trials !== null && order.trials.length > 0).length, attempted: measured.length, verified: verified.length,
      quality: measured.length ? verified.length / measured.length * 100 : null,
      task_time: quantiles(verified.map(trial => trial.elapsed_ms)), ttft: quantiles(verified.map(trial => trial.first_content_ms)) };
  });
}

export function renderPooledReport(entries, { systems = DEFAULT_SYSTEMS, sanity } = {}) {
  const pooled = pooledData(entries, { systems });
  const title = 'FreeLlama benchmark · pooled run orders';
  const pair = (pair) => pair.n ? `${esc(format(pair.p50))} / ${esc(format(pair.p95))}<small>n=${pair.n}</small>` : '<span class="missing" role="img" aria-label="Not measured">—</span>';
  const rows = pooled.map(system => `<tr><th scope="row">${esc(system.name)}<small>${system.orders.map(order => `${esc(order.label)}: ${esc(statusLabel(order.status))}${order.raw_trials === null ? ' · no raw trials' : ''}`).join(' · ')}</small></th><td>${system.orders_pooled} of ${entries.length}</td><td>${system.verified} / ${system.attempted}</td><td class="${system.quality === null ? 'unknown' : 'measured'}">${system.quality === null ? '<span class="missing" role="img" aria-label="Not measured">—</span>' : esc(format(system.quality))}</td><td class="${system.task_time.n ? 'measured' : 'unknown'}">${pair(system.task_time)}</td><td class="${system.ttft.n ? 'measured' : 'unknown'}">${pair(system.ttft)}</td></tr>`).join('');
  const perOrder = entries.map((entry, index) => { const data = dashboardData(entry.report, { systems }); return `<section aria-labelledby="order-${index}"><h2 id="order-${index}">Order ${index + 1}: ${esc(entry.label)}</h2><p class="subhead">${esc(data.date)}${data.date ? ' · ' : ''}${esc(data.scope)}</p>${commonTable(data)}</section>`; }).join('');
  const data = { title, orders: entries.map(entry => entry.label), pooled };
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${esc(title)}</title><style>
${STYLE}
</style></head><body><main><header><h1>${esc(title)}</h1><p class="verdict">${entries.length} run orders. No ranking or winner is inferred.</p><p class="subhead">Pooled p50/p95 use every verified measured trial from each order's raw trials.jsonl; warm-ups are excluded.</p></header><section aria-labelledby="pooled-heading"><h2 id="pooled-heading">Pooled across orders</h2><p class="note">Compare the gap between systems with the drift between orders before claiming a difference. — means Not measured.</p><div class="table-scroll"><table><thead><tr><th scope="col">System / per-order status</th><th scope="col">Orders pooled</th><th scope="col">Verified / attempted</th><th scope="col">Quality<small>% correct</small></th><th scope="col">Task time<small>ms · p50 / p95</small></th><th scope="col">TTFT<small>ms · p50 / p95</small></th></tr></thead><tbody>${rows}</tbody></table></div></section>${perOrder}${sanityHtml(normalizeSanity(sanity))}<footer>Evidence report. No ranking, aggregate score, estimated result, or winner is inferred. This file contains no external requests or dependencies.</footer></main><script id="benchmark-data" type="application/json">${safeJson(data)}</script></body></html>`;
}

// Raw trials live in each vendor's directory next to summary.json.
export async function loadRunEntry(file) {
  const report = JSON.parse(await readFile(file, 'utf8'));
  const trials = {};
  for (const vendor of Array.isArray(report.vendors) ? report.vendors : []) {
    if (typeof vendor?.id !== 'string' || !/^[a-z0-9][a-z0-9_-]*$/.test(vendor.id)) continue;
    try { trials[vendor.id] = (await readFile(path.join(path.dirname(file), vendor.id, 'trials.jsonl'), 'utf8')).split('\n').filter(Boolean).map(line => JSON.parse(line)); }
    catch (error) { if (error.code !== 'ENOENT') throw error; trials[vendor.id] = null; }
  }
  return { label: path.basename(path.dirname(file)), file, report, trials };
}

async function main() {
  const { values } = parseArgs({ options: { report: { type: 'string', multiple: true }, sanity: { type: 'string' }, out: { type: 'string' }, help: { type: 'boolean' } } });
  if (values.help || !values.report?.length || !values.out) {
    console.log('Usage: node scripts/bench/report.mjs --report FILE [--report FILE ...] [--sanity FILE] --out NEW_FILE.html\nRenders the benchmarked systems and six common KPIs. Comparison plots require report.comparability.verified=true or explicitly qualified per-system cohorts. Repeat --report for run orders: the page then shows a pooled table (p50/p95 from each run\'s raw trials.jsonl) and one table per order. No services or inference run.');
    if (!values.help) process.exitCode = 2;
    return;
  }
  const sanity = values.sanity ? JSON.parse(await readFile(path.resolve(values.sanity), 'utf8')) : undefined;
  const out = path.resolve(values.out);
  let html;
  if (values.report.length === 1) html = renderDashboard(JSON.parse(await readFile(path.resolve(values.report[0]), 'utf8')), { sanity });
  else html = renderPooledReport(await Promise.all(values.report.map(file => loadRunEntry(path.resolve(file)))), { sanity });
  await mkdir(path.dirname(out), { recursive: true });
  await writeFile(out, html, { flag: 'wx' });
  console.log(out);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(error => { console.error(error.message); process.exitCode = 1; });
