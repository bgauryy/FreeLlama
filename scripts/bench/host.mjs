#!/usr/bin/env node
// Observation only. Missing OS counters remain null and retain their diagnostic.
import { execFile } from 'node:child_process';
import { appendFile, readFile } from 'node:fs/promises';
import os from 'node:os';
import { parseArgs, promisify } from 'node:util';
import { pathToFileURL } from 'node:url';
import { summarizeRss } from './resources.mjs';

const exec = promisify(execFile);
async function command(file, args, errors) {
  try { return (await exec(file, args, { timeout: 2000 })).stdout; }
  catch (error) { errors.push(`${file} ${args.join(' ')}: ${error.stderr?.trim() || error.message}`); return ''; }
}
// Same bounded estimate as the core (platform/resources/sampling.rs parse_macos): free +
// max(min(inactive + speculative, file-backed), purgeable), capped at total RAM. These vm_stat
// counters overlap, so summing them would inflate headroom. It is an estimate, not a kernel value.
export function macAvailableEstimate({ free, inactive, speculative, fileBacked, purgeable, total = null }) {
  if ([free, inactive, speculative, fileBacked, purgeable].some(value => value === null)) return null;
  const available = free + Math.max(Math.min(inactive + speculative, fileBacked), purgeable);
  return typeof total === 'number' && total > 0 ? Math.min(available, total) : available;
}
export function parseMacMemory(vm, swap, pressure, totalBytes = null) {
  const page = Number(vm.match(/page size of (\d+) bytes/)?.[1]);
  const count = (name) => { const m = vm.match(new RegExp(`^${name}:\\s+(\\d+)`, 'm')); return m && page ? Number(m[1]) * page : null; };
  const free = count('Pages free'), speculative = count('Pages speculative');
  const used = swap.match(/used\s*=\s*([\d.]+)([KMGT])?/i);
  const level = Number(pressure.trim());
  const available = macAvailableEstimate({ free, inactive: count('Pages inactive'), speculative, fileBacked: count('File-backed pages'), purgeable: count('Pages purgeable'), total: totalBytes });
  return {
    available_bytes: available,
    available_bytes_source: available === null ? null : 'freellama_estimate',
    free_bytes: free === null || speculative === null ? null : free + speculative,
    compressor_bytes: count('Pages occupied by compressor'),
    swap_used_bytes: used ? Number(used[1]) * (1024 ** ({ K: 1, M: 2, G: 3, T: 4 }[used[2]?.toUpperCase()] ?? 0)) : null,
    pressure_level: [1, 2, 4].includes(level) ? level : null,
  };
}
export function parseLinuxMemory(mem, psi) {
  const kb = (key) => { const m = mem.match(new RegExp(`^${key}:\\s+(\\d+) kB`, 'm')); return m ? Number(m[1]) * 1024 : null; };
  const total = kb('SwapTotal'), free = kb('SwapFree');
  const avg = psi.match(/^some\s+avg10=([\d.]+)/m);
  const available = kb('MemAvailable');
  return { available_bytes: available, available_bytes_source: available === null ? null : 'linux_memavailable', free_bytes: kb('MemFree'), swap_used_bytes: total === null || free === null || free > total ? null : total - free, psi_some_avg10_pct: avg ? Number(avg[1]) : null };
}
// Aggregate scheduler counters over all logical CPUs; these are not process CPU time.
export function cpuSnapshot(cpus) {
  if (!Array.isArray(cpus) || !cpus.length) return null;
  let idle_ms = 0, total_ms = 0;
  for (const cpu of cpus) {
    const values = ['user', 'nice', 'sys', 'idle', 'irq'].map(key => cpu?.times?.[key]);
    if (values.some(value => typeof value !== 'number' || !Number.isFinite(value) || value < 0)) return null;
    idle_ms += cpu.times.idle;
    total_ms += values.reduce((sum, value) => sum + value, 0);
  }
  return { logical_cpus: cpus.length, idle_ms, total_ms };
}
export function cpuBusyPercent(before, after) {
  const valid = value => value && Number.isInteger(value.logical_cpus) && value.logical_cpus > 0 && typeof value.total_ms === 'number' && Number.isFinite(value.total_ms) && typeof value.idle_ms === 'number' && Number.isFinite(value.idle_ms) && value.total_ms >= value.idle_ms && value.idle_ms >= 0;
  if (!valid(before) || !valid(after) || before.logical_cpus !== after.logical_cpus) return null;
  const total = after.total_ms - before.total_ms, idle = after.idle_ms - before.idle_ms;
  if (!Number.isFinite(total) || !Number.isFinite(idle) || total <= 0 || idle < 0 || idle > total) return null;
  return 100 * (1 - idle / total);
}
export async function sampleHost({ processSampler } = {}) {
  const errors = [];
  const timestamp_ms = Date.now();
  let cpu_times = null;
  try { cpu_times = cpuSnapshot(os.cpus()); } catch (error) { errors.push(`CPU counters: ${error.message}`); }
  const sample = { timestamp_ms, platform: process.platform, cpu_times, cpu_busy_percent: null, available_bytes: null, available_bytes_source: null, free_bytes: null, swap_used_bytes: null, pressure_level: null, psi_some_avg10_pct: null, compressor_bytes: null, load1: os.loadavg()[0], errors };
  if (!sample.cpu_times) errors.push('CPU cumulative scheduler counters unavailable');
  if (process.platform === 'darwin') {
    const vm = await command('/usr/bin/vm_stat', [], errors);
    const swap = await command('/usr/sbin/sysctl', ['-n', 'vm.swapusage'], errors);
    const pressure = await command('/usr/sbin/sysctl', ['-n', 'kern.memorystatus_vm_pressure_level'], errors);
    Object.assign(sample, parseMacMemory(vm, swap, pressure, os.totalmem()));
  } else if (process.platform === 'linux') {
    const read = async (file) => { try { return await readFile(file, 'utf8'); } catch (error) { errors.push(`${file}: ${error.message}`); return ''; } };
    Object.assign(sample, parseLinuxMemory(await read('/proc/meminfo'), await read('/proc/pressure/memory')));
  } else errors.push('Host memory sampler is not implemented on this platform');
  const expected = process.platform === 'darwin' ? ['available_bytes', 'free_bytes', 'compressor_bytes', 'swap_used_bytes', 'pressure_level'] : process.platform === 'linux' ? ['available_bytes', 'free_bytes', 'swap_used_bytes', 'psi_some_avg10_pct'] : [];
  for (const key of expected) if (sample[key] === null || !Number.isFinite(sample[key]) || sample[key] < 0) { sample[key] = null; errors.push(`Counter unavailable or invalid: ${key}`); }
  if (processSampler) { try { sample.processes = await processSampler(); } catch (error) { sample.processes = { error: String(error.message ?? error) }; } }
  sample.sample_duration_ms = Date.now() - sample.timestamp_ms;
  return sample;
}
export function summarizeHost(samples) {
  const numbers = (field) => samples.map(s => s[field]).filter(v => typeof v === 'number' && Number.isFinite(v));
  const available = numbers('available_bytes'), swap = numbers('swap_used_bytes');
  const firstSwap = samples[0]?.swap_used_bytes, lastSwap = samples.at(-1)?.swap_used_bytes;
  const good = (v) => typeof v === 'number' && Number.isFinite(v);
  const cpu = { time_weighted_mean: null, peak: null, covered_ms: 0, unknown_ms: 0, coverage: null, scope: 'Whole host across all logical CPU capacity; includes other processes and warm-ups; not process CPU or load average' };
  let weightedCpu = 0;
  const duration = { warning_ms: null, critical_ms: null, linux_psi_above_10pct_ms: null, covered_ms: 0, unsampled_gap_ms: 0, macos_pressure_covered_ms: 0, macos_pressure_unknown_ms: 0, linux_psi_covered_ms: 0, linux_psi_unknown_ms: 0 };
  for (let i = 0; i + 1 < samples.length; i++) {
    const s = samples[i], ms = samples[i + 1].timestamp_ms - s.timestamp_ms;
    if (Number.isFinite(ms) && ms > 0) {
      const busy = cpuBusyPercent(s.cpu_times, samples[i + 1].cpu_times);
      if (busy === null) cpu.unknown_ms += ms;
      else { cpu.covered_ms += ms; weightedCpu += busy * ms; cpu.peak = cpu.peak === null ? busy : Math.max(cpu.peak, busy); }
    }
    if (ms <= 0 || ms > 5000) { if (ms > 0) duration.unsampled_gap_ms += ms; continue; }
    duration.covered_ms += ms;
    if ([1, 2, 4].includes(s.pressure_level)) {
      duration.macos_pressure_covered_ms += ms;
      duration.warning_ms ??= 0; duration.critical_ms ??= 0;
      if (s.pressure_level === 2) duration.warning_ms += ms;
      if (s.pressure_level === 4) duration.critical_ms += ms;
    } else duration.macos_pressure_unknown_ms += ms;
    if (good(s.psi_some_avg10_pct)) { duration.linux_psi_covered_ms += ms; duration.linux_psi_above_10pct_ms ??= 0; if (s.psi_some_avg10_pct > 10) duration.linux_psi_above_10pct_ms += ms; }
    else duration.linux_psi_unknown_ms += ms;
  }
  if (cpu.covered_ms > 0) cpu.time_weighted_mean = weightedCpu / cpu.covered_ms;
  if (cpu.covered_ms + cpu.unknown_ms > 0) cpu.coverage = cpu.covered_ms / (cpu.covered_ms + cpu.unknown_ms);
  const sources = [...new Set(samples.filter(s => good(s.available_bytes)).map(s => s.available_bytes_source ?? 'unlabelled'))];
  const rss = samples.some(s => s.processes) ? { process_rss: summarizeRss(samples) } : {};
  return { sample_count: samples.length, cpu_busy_percent: cpu, min_available_bytes: available.length ? Math.min(...available) : null, available_bytes_source: sources.length ? sources.join('+') : null, ...rss, peak_swap_growth_bytes: good(firstSwap) && swap.length ? Math.max(...swap) - firstSwap : null, ending_swap_growth_bytes: good(firstSwap) && good(lastSwap) ? lastSwap - firstSwap : null, sampled_pressure: duration, samples_with_errors: samples.filter(s => s.errors?.length).length };
}
export async function startHostSampler(outputFile, { intervalMs = 1000, processSampler } = {}) {
  let stopped = false, timer;
  let current = Promise.resolve();
  let previous = null;
  const tick = async () => {
    const sample = await sampleHost({ processSampler });
    sample.cpu_busy_percent = cpuBusyPercent(previous?.cpu_times, sample.cpu_times);
    previous = sample;
    await appendFile(outputFile, JSON.stringify(sample) + '\n');
    if (!stopped) timer = setTimeout(() => { current = tick(); current.catch(() => {}); }, intervalMs);
  };
  current = tick();
  await current;
  return async () => { stopped = true; clearTimeout(timer); await current; };
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { values } = parseArgs({ options: { out: { type: 'string' }, help: { type: 'boolean' } } });
  if (values.help || !values.out) console.log('Usage: node scripts/bench/host.mjs --out EXISTING_DIRECTORY/host.jsonl');
  else {
    const stop = await startHostSampler(values.out);
    for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, async () => { await stop(); process.exit(0); });
  }
}
