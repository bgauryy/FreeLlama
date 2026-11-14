// Process RSS, load and Ollama residency observations, plus the one lifecycle action the harness
// owns: unloading a benchmark model that this run warmed. Never stops or unloads anything else.
import { execFile } from 'node:child_process';
import os from 'node:os';
import { promisify } from 'node:util';

const exec = promisify(execFile);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const good = value => typeof value === 'number' && Number.isFinite(value) && value >= 0;
export const OLLAMA_BACKED = new Set(['ollama', 'freellama', 'freellama-managed', 'freellama-stream']);
export const DEFAULT_OLLAMA_BASE_URL = 'http://127.0.0.1:11434';
export const RSS_SCOPE = 'Resident set size of the owned process group and Ollama runner processes. On Apple unified memory RSS undercounts GPU buffers; it is reported beside Ollama size_vram and never added to it.';

// `ps -A -o pid=,ppid=,pgid=,rss=,args=`: rss is KiB on macOS and Linux.
export function parseProcessTable(text) {
  const rows = [];
  for (const line of String(text ?? '').split('\n')) {
    const m = line.match(/^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/);
    if (m) rows.push({ pid: Number(m[1]), ppid: Number(m[2]), pgid: Number(m[3]), rss_bytes: Number(m[4]) * 1024, args: m[5].trim() });
  }
  return rows;
}
// Current Ollama runs `ollama runner ...`; older releases used `ollama_llama_server`.
export function isOllamaRunner(args) {
  const [argv0 = '', argv1 = ''] = String(args ?? '').trim().split(/\s+/);
  const base = argv0.split('/').at(-1);
  return base === 'ollama_llama_server' || base === 'ollama' && argv1 === 'runner';
}
export function summarizeProcessRss(rows, { ownedPgid = null, includeOllamaRunners = false } = {}) {
  const owned = ownedPgid ? rows.filter(row => row.pgid === ownedPgid) : [];
  const runners = includeOllamaRunners ? rows.filter(row => isOllamaRunner(row.args)) : [];
  const sum = list => list.reduce((total, row) => total + row.rss_bytes, 0);
  const tracked = new Map([...owned, ...runners].map(row => [row.pid, row]));
  return {
    owned_pgid: ownedPgid, owned_pids: owned.map(row => row.pid), owned_rss_bytes: ownedPgid ? sum(owned) : null,
    ollama_runner_pids: runners.map(row => row.pid), ollama_runner_rss_bytes: includeOllamaRunners ? sum(runners) : null,
    // Distinct processes only: a runner inside the owned group is counted once.
    tracked_rss_bytes: ownedPgid || includeOllamaRunners ? sum([...tracked.values()]) : null,
  };
}
export async function sampleProcessRss({ ownedPgid = null, includeOllamaRunners = false, execImpl = exec } = {}) {
  if (!ownedPgid && !includeOllamaRunners) return null;
  try {
    const { stdout } = await execImpl('ps', ['-A', '-o', 'pid=,ppid=,pgid=,rss=,args='], { timeout: 2000, maxBuffer: 8 * 1024 * 1024 });
    return summarizeProcessRss(parseProcessTable(stdout), { ownedPgid, includeOllamaRunners });
  } catch (error) { return { owned_pgid: ownedPgid, owned_rss_bytes: null, ollama_runner_rss_bytes: null, tracked_rss_bytes: null, error: `ps: ${error.message}` }; }
}
export function summarizeRss(samples) {
  const peak = key => { const values = samples.map(sample => sample.processes?.[key]).filter(good); return values.length ? Math.max(...values) : null; };
  return { peak_tracked_bytes: peak('tracked_rss_bytes'), peak_owned_bytes: peak('owned_rss_bytes'), peak_ollama_runner_bytes: peak('ollama_runner_rss_bytes'), samples_with_process_data: samples.filter(sample => good(sample.processes?.tracked_rss_bytes)).length, scope: RSS_SCOPE };
}

// `ps -Ao pcpu,comm -r` (macOS) or `ps -eo pcpu,comm --sort=-pcpu` (Linux), header included.
export function parseTopCpu(text, limit = 5) {
  const rows = [];
  for (const line of String(text ?? '').split('\n')) {
    const m = line.match(/^\s*(\d+(?:\.\d+)?)\s+(.+?)\s*$/);
    if (m) rows.push({ pcpu: Number(m[1]), command: m[2] });
  }
  return rows.sort((a, b) => b.pcpu - a.pcpu).slice(0, limit);
}
export async function loadSnapshot({ execImpl = exec, platform = process.platform, loadavg = os.loadavg(), logicalCpus = os.cpus().length } = {}) {
  const [load1, load5, load15] = loadavg;
  const snapshot = { timestamp: new Date().toISOString(), load1, load5, load15, logical_cpus: logicalCpus, load1_per_cpu: good(load1) && logicalCpus > 0 ? load1 / logicalCpus : null, top_processes: [], error: null };
  try {
    const args = platform === 'darwin' ? ['-Ao', 'pcpu,comm', '-r'] : ['-eo', 'pcpu,comm', '--sort=-pcpu'];
    snapshot.top_processes = parseTopCpu((await execImpl('ps', args, { timeout: 2000, maxBuffer: 8 * 1024 * 1024 })).stdout);
  } catch (error) { snapshot.error = `ps: ${error.message}`; }
  return snapshot;
}
export function evaluateLoadGate(snapshot, maxPerCpu, strict = false) {
  const value = snapshot?.load1_per_cpu;
  const above = good(value) ? value > maxPerCpu : null;
  return { max_load_per_cpu: maxPerCpu, strict, above, action: above === null ? 'unknown' : !above ? 'ok' : strict ? 'refuse' : 'warn' };
}

async function smallText(response, limit = 1024 * 1024) {
  if (!response.body) return '';
  const reader = response.body.getReader(), decoder = new TextDecoder();
  let bytes = 0, text = '';
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return text + decoder.decode();
      bytes += value.byteLength;
      if (bytes > limit) { await reader.cancel().catch(() => {}); throw new Error(`Ollama response exceeded ${limit} bytes`); }
      text += decoder.decode(value, { stream: true });
    }
  } finally { reader.releaseLock(); }
}
const ollamaUrl = (base, route) => `${String(base).replace(/\/$/, '')}${route}`;
// /api/ps residency. size_vram is the GPU share Ollama reports; it is never added to RSS.
export async function ollamaResidency(baseUrl, model, { fetchImpl = fetch, signal } = {}) {
  try {
    const response = await fetchImpl(ollamaUrl(baseUrl, '/api/ps'), { redirect: 'error', signal: AbortSignal.any([AbortSignal.timeout(5000), ...(signal ? [signal] : [])]) });
    const text = await smallText(response);
    if (!response.ok) return { model, resident: null, error: `HTTP ${response.status}: ${text.slice(0, 300)}` };
    const json = JSON.parse(text);
    if (!Array.isArray(json?.models)) return { model, resident: null, error: '/api/ps has no models array' };
    const entry = json.models.find(item => item && [item.name, item.model].includes(model));
    return { model, resident: !!entry, size_bytes: good(entry?.size) ? entry.size : null, size_vram_bytes: good(entry?.size_vram) ? entry.size_vram : null, expires_at: entry?.expires_at ?? null, resident_models: json.models.length, error: null };
  } catch (error) { return { model, resident: null, error: String(error.message ?? error) }; }
}
// Only called for the benchmark model this run warmed. keep_alive:0 asks Ollama to unload it;
// /api/ps then has to confirm that the model is gone.
export async function unloadOllamaModel(baseUrl, model, { fetchImpl = fetch, signal, confirmTimeoutMs = 10_000, pollMs = 250, now = () => Date.now(), wait = sleep } = {}) {
  const started = now();
  const outcome = { model, requested: true, http_status: null, confirmed_absent: false, elapsed_ms: null, residency: null, error: null };
  try {
    const response = await fetchImpl(ollamaUrl(baseUrl, '/api/generate'), { method: 'POST', redirect: 'error', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ model, keep_alive: 0 }), signal: AbortSignal.any([AbortSignal.timeout(30_000), ...(signal ? [signal] : [])]) });
    outcome.http_status = response.status;
    const text = await smallText(response);
    if (!response.ok) throw new Error(`unload HTTP ${response.status}: ${text.slice(0, 300)}`);
    for (;;) {
      outcome.residency = await ollamaResidency(baseUrl, model, { fetchImpl, signal });
      if (outcome.residency.resident === false) { outcome.confirmed_absent = true; break; }
      if (now() - started >= confirmTimeoutMs) { outcome.error = outcome.residency.error ?? 'Model still listed by /api/ps after unload'; break; }
      await wait(pollMs);
    }
  } catch (error) { outcome.error = String(error.message ?? error); }
  outcome.elapsed_ms = now() - started;
  return outcome;
}
