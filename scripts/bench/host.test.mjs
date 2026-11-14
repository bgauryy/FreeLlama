import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cpuBusyPercent, cpuSnapshot, parseMacMemory, parseLinuxMemory, summarizeHost } from './host.mjs';

test('macOS respects page size and distinguishes free from unavailable effective memory', () => {
  const m = parseMacMemory('Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages speculative: 2.\nPages occupied by compressor: 7.\n', 'total = 5G used = 1.5G free = 3.5G', '2');
  assert.equal(m.available_bytes, null);
  assert.equal(m.free_bytes, 12 * 16384);
  assert.equal(m.compressor_bytes, 7 * 16384);
  assert.equal(m.swap_used_bytes, 1.5 * 1024 ** 3);
  assert.equal(m.pressure_level, 2);
});
test('Linux measures swap occupancy rather than swap I/O counters', () => {
  const m = parseLinuxMemory('MemAvailable: 1000 kB\nMemFree: 50 kB\nSwapTotal: 200 kB\nSwapFree: 125 kB\n', 'some avg10=12.50 avg60=1.0 total=34');
  assert.equal(m.available_bytes, 1024000);
  assert.equal(m.swap_used_bytes, 75 * 1024);
  assert.equal(m.psi_some_avg10_pct, 12.5);
});
test('missing counters are unknown, never a healthy zero', () => {
  assert.equal(parseMacMemory('', '', '').swap_used_bytes, null);
  assert.equal(parseMacMemory('', '', '').pressure_level, null);
  assert.equal(parseLinuxMemory('', '').swap_used_bytes, null);
  assert.equal(parseLinuxMemory('', '').available_bytes, null);
  assert.equal(parseLinuxMemory('SwapTotal: 100 kB\nSwapFree: 200 kB\n', '').swap_used_bytes, null);
});
test('peak swap and interval-weighted pressure retain missing telemetry', () => {
  const result = summarizeHost([
    { timestamp_ms: 0, available_bytes: null, swap_used_bytes: 10, pressure_level: 2 },
    { timestamp_ms: 1200, available_bytes: null, swap_used_bytes: 30, pressure_level: 4 },
    { timestamp_ms: 3000, available_bytes: null, swap_used_bytes: 15, pressure_level: 1 },
  ]);
  assert.equal(result.min_available_bytes, null);
  assert.equal(result.peak_swap_growth_bytes, 20);
  assert.equal(result.ending_swap_growth_bytes, 5);
  assert.equal(result.sampled_pressure.warning_ms, 1200);
  assert.equal(result.sampled_pressure.critical_ms, 1800);
  assert.equal(result.sampled_pressure.linux_psi_above_10pct_ms, null);
});

test('pressure duration reports its own missing-telemetry coverage', () => {
  const result = summarizeHost([
    { timestamp_ms: 0, pressure_level: 1, psi_some_avg10_pct: null },
    { timestamp_ms: 1000, pressure_level: null, psi_some_avg10_pct: 15 },
    { timestamp_ms: 3000, pressure_level: null, psi_some_avg10_pct: null },
  ]);
  assert.equal(result.sampled_pressure.warning_ms, 0);
  assert.equal(result.sampled_pressure.macos_pressure_covered_ms, 1000);
  assert.equal(result.sampled_pressure.macos_pressure_unknown_ms, 2000);
  assert.equal(result.sampled_pressure.linux_psi_covered_ms, 2000);
  assert.equal(result.sampled_pressure.linux_psi_unknown_ms, 1000);
  assert.equal(result.sampled_pressure.linux_psi_above_10pct_ms, 2000);
});

test('CPU busy percentage uses scheduler deltas across logical capacity, not load average', () => {
  const times = (user, idle) => ({ times: { user, nice: 0, sys: 0, irq: 0, idle } });
  const before = cpuSnapshot([times(100, 900), times(100, 900)]);
  const after = cpuSnapshot([times(600, 1400), times(100, 1900)]);
  assert.deepEqual(before, { logical_cpus: 2, total_ms: 2000, idle_ms: 1800 });
  assert.equal(cpuBusyPercent(before, after), 25);
  assert.equal(cpuBusyPercent(after, before), null);
  assert.equal(cpuBusyPercent(before, { ...after, logical_cpus: 1 }), null);
  assert.equal(cpuBusyPercent(before, before), null);
  assert.equal(cpuSnapshot([]), null);
  assert.equal(cpuSnapshot([{ times: { user: -1 } }]), null);
});

test('CPU average is time weighted and reports missing-counter coverage', () => {
  const snapshot = (total_ms, idle_ms) => ({ logical_cpus: 2, total_ms, idle_ms });
  const result = summarizeHost([
    { timestamp_ms: 0, load1: 100, cpu_times: snapshot(1000, 500) },
    { timestamp_ms: 1000, load1: 0, cpu_times: snapshot(3000, 2000) },
    { timestamp_ms: 4000, load1: 0, cpu_times: snapshot(9000, 2000) },
    { timestamp_ms: 5000, load1: 100, cpu_times: null },
  ]);
  assert.equal(result.cpu_busy_percent.time_weighted_mean, 81.25);
  assert.equal(result.cpu_busy_percent.peak, 100);
  assert.equal(result.cpu_busy_percent.covered_ms, 4000);
  assert.equal(result.cpu_busy_percent.unknown_ms, 1000);
  assert.equal(result.cpu_busy_percent.coverage, 0.8);
  assert.equal(summarizeHost([{ timestamp_ms: 0, load1: 100 }, { timestamp_ms: 1000, load1: 100 }]).cpu_busy_percent.time_weighted_mean, null);
  assert.equal(summarizeHost([{ timestamp_ms: 0 }]).cpu_busy_percent.coverage, null);
});
