#!/usr/bin/env node
// Native CPU sanity only. This never starts a server or downloads a model.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { mkdir, open, readFile, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { acquireLock, DEFAULT_LOCK_PATH } from './lib.mjs';
import { startHostSampler, summarizeHost } from './host.mjs';

export function parseProcessTime(text) {
  const field = name => { const found = [...text.matchAll(new RegExp(`^${name}\\s+([0-9]+(?:\\.[0-9]+)?)\\s*$`, 'gm'))].at(-1); return found ? Number(found[1]) : null; };
  const wall = field('real'), user = field('user'), system = field('sys');
  return { wall_seconds: wall, user_seconds: user, system_seconds: system,
    cpu_core_equivalents: wall > 0 && user !== null && system !== null ? (user + system) / wall : null,
    peak_rss_bytes: null,
    scope: 'Whole native command, including startup, loading, warmup and measured cases. CPU cores = (user + system seconds) / wall seconds. Peak process RSS is not collected.' };
}

async function run(options) {
  if (process.platform === 'win32') throw Error('Native sanity ownership currently requires POSIX process groups');
  if (!options.model || !options.digest || !options.out) throw Error('--model, --digest and --out are required');
  if (!/^[a-f0-9]{64}$/.test(options.digest)) throw Error('--digest must be an exact SHA-256');
  const repetitions = Number(options.repetitions ?? 5), threads = Number(options.threads ?? 4);
  if (!Number.isSafeInteger(repetitions) || repetitions < 2 || repetitions > 100) throw Error('--repetitions must be 2..100');
  if (!Number.isSafeInteger(threads) || threads < 1 || threads > os.cpus().length) throw Error('--threads must fit the host logical CPU count');
  const out = path.resolve(options.out), model = path.resolve(options.model);
  const unlock = await acquireLock(DEFAULT_LOCK_PATH);
  let stopSampler = async () => {}, stdout, stderr, child;
  let interrupted = false, timedOut = false, timer, killTimer;
  const terminate = signal => { try { if (child?.pid) process.kill(-child.pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; } };
  const interrupt = () => { interrupted = true; terminate('SIGTERM'); clearTimeout(killTimer); killTimer = setTimeout(() => terminate('SIGKILL'), 5000); };
  process.once('SIGINT', interrupt); process.once('SIGTERM', interrupt);
  try {
    await mkdir(path.dirname(out), { recursive: true }); await mkdir(out);
    if (!(await stat(model)).isFile()) throw Error('Selected model must be a regular file');
    const hash = createHash('sha256'); for await (const bytes of createReadStream(model)) { if (interrupted) throw Error('Cancelled while verifying model'); hash.update(bytes); }
    if (hash.digest('hex') !== options.digest) throw Error('Selected model SHA-256 does not match');
    if (interrupted) throw Error('Cancelled before native inference started');
    const binary = options.binary ?? 'llama-bench';
    const args = ['--offline', '-m', model, '-p', '128', '-n', '64', '-r', String(repetitions), '-t', String(threads), '-ngl', '0', '-dev', 'none', '-nkvo', '1', '-nopo', '1', '-o', 'json'];
    const metadata = { started_at: new Date().toISOString(), command: '/usr/bin/time', args: ['-p', binary, ...args],
      model_sha256: options.digest, platform: process.platform, scope: 'CPU synthetic sanity, not a matched vendor/task-quality comparison' };
    await writeFile(path.join(out, 'command.json'), JSON.stringify(metadata, null, 2) + '\n');
    stdout = await open(path.join(out, 'raw.json'), 'wx'); stderr = await open(path.join(out, 'stderr.log'), 'wx');
    stopSampler = await startHostSampler(path.join(out, 'host.jsonl'));
    child = spawn('/usr/bin/time', metadata.args, { detached: true, shell: false, env: { ...process.env, LC_ALL: 'C' }, stdio: ['ignore', stdout.fd, stderr.fd] });
    timer = setTimeout(() => { timedOut = true; terminate('SIGTERM'); killTimer = setTimeout(() => terminate('SIGKILL'), 5000); }, 180000);
    const outcome = await new Promise(resolve => { child.once('error', error => resolve({ error: error.message })); child.once('close', (code, signal) => resolve({ code, signal })); });
    clearTimeout(timer); clearTimeout(killTimer);
    if (interrupted || timedOut || outcome.code !== 0) terminate('SIGKILL');
    await stopSampler(); stopSampler = async () => {};
    await stdout.close(); stdout = null; await stderr.close(); stderr = null;
    let rows = null, parse_error = null;
    try { rows = JSON.parse(await readFile(path.join(out, 'raw.json'), 'utf8')); } catch (error) { parse_error = error.message; }
    const complete = outcome.code === 0 && !interrupted && !timedOut && Array.isArray(rows) && rows.length === 2
      && rows.every(row => row.samples_ts?.length === repetitions && row.samples_ns?.length === repetitions
        && row.n_gpu_layers === 0 && row.devices === 'none'
        && row.samples_ts.every(value => Number.isFinite(value) && value > 0)
        && row.samples_ns.every(value => Number.isFinite(value) && value > 0))
      && rows[0].n_prompt === 128 && rows[0].n_gen === 0 && rows[1].n_prompt === 0 && rows[1].n_gen === 64;
    const host = (await readFile(path.join(out, 'host.jsonl'), 'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse);
    const result = { ...metadata, finished_at: new Date().toISOString(), status: complete ? 'complete_sanity' : 'failed_sanity', outcome, timed_out: timedOut,
      interrupted, parse_error, rows, resources: parseProcessTime(await readFile(path.join(out, 'stderr.log'), 'utf8')), host_observations: summarizeHost(host) };
    await writeFile(path.join(out, 'summary.json'), JSON.stringify(result, null, 2) + '\n');
    console.log(JSON.stringify({ out, status: result.status, resources: result.resources, means_tps: Array.isArray(rows) ? rows.map(row => row.avg_ts) : null }, null, 2));
    if (!complete) process.exitCode = 1;
  } finally {
    clearTimeout(timer); clearTimeout(killTimer); process.removeListener('SIGINT', interrupt); process.removeListener('SIGTERM', interrupt);
    if (child && child.exitCode === null && child.signalCode === null) terminate('SIGKILL');
    try { await stopSampler(); } finally { await stdout?.close(); await stderr?.close(); await unlock(); }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { values } = parseArgs({ options: { model: { type: 'string' }, digest: { type: 'string' }, out: { type: 'string' }, binary: { type: 'string' }, repetitions: { type: 'string' }, threads: { type: 'string' }, help: { type: 'boolean' } } });
  if (values.help) console.log('node scripts/bench/sanity.mjs --model VERIFIED_GGUF --digest SHA256 --out NEW_DIRECTORY [--repetitions 5] [--threads 4]');
  else run(values).catch(error => { console.error(error.message); process.exitCode = 1; });
}
