#!/usr/bin/env node
// C2 helper: allocates and touches N GiB, prints one ready line, then holds until SIGTERM/SIGINT.
// Refuses when N exceeds estimated available memory minus 2 GiB, unless --force.
import { spawn } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { sampleHost } from './host.mjs';

export const GIB = 1024 ** 3;
export const HEADROOM_BYTES = 2 * GIB;
export const CHUNK_BYTES = 64 * 1024 ** 2;
export const MAX_HOG_GB = 1024;
const SCRIPT = fileURLToPath(import.meta.url);

export function parseHogGb(value) {
  const gb = typeof value === 'number' ? value : Number(value);
  if (value === undefined || value === null || value === '' || !Number.isFinite(gb) || gb <= 0 || gb > MAX_HOG_GB) throw new Error(`--hog-gb must be a number above 0 and at most ${MAX_HOG_GB}.`);
  return gb;
}
export function parseHogArgs(argv) {
  const options = { gb: undefined, force: false };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === '--force') options.force = true;
    else if (arg === '--gb') options.gb = parseHogGb(argv[++index]);
    else throw new Error(`Unknown argument: ${arg}`);
  }
  if (options.gb === undefined) throw new Error('--gb is required.');
  return options;
}
// Unknown available memory refuses too: the guard must not pass on missing telemetry.
export function hogDecision({ gb, availableBytes, force = false }) {
  const requested = Math.ceil(parseHogGb(gb) * GIB);
  const known = typeof availableBytes === 'number' && Number.isFinite(availableBytes) && availableBytes >= 0;
  const limit = known ? Math.max(0, availableBytes - HEADROOM_BYTES) : null;
  const fits = known && requested <= limit;
  return { requested_bytes: requested, available_bytes: known ? availableBytes : null, limit_bytes: limit, headroom_bytes: HEADROOM_BYTES, forced: force && !fits, allowed: fits || force,
    reason: fits ? null : !known ? 'Estimated available memory is unknown' : `Requested ${requested} bytes exceeds estimated available minus 2 GiB (${limit} bytes)` };
}
// Writing every byte touches each page, so the allocation is resident rather than reserved.
export function allocateTouched(bytes, chunkBytes = CHUNK_BYTES) {
  const chunks = [];
  for (let left = bytes; left > 0; left -= chunkBytes) chunks.push(Buffer.allocUnsafe(Math.min(chunkBytes, left)).fill(0x5a));
  return chunks;
}

// Runner side: start the helper as a child of this process and wait for its ready line.
export function startMemoryHog({ gb, force = false, timeoutMs = 120_000, script = SCRIPT, signal } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [script, '--gb', String(gb), ...(force ? ['--force'] : [])], { stdio: ['ignore', 'pipe', 'pipe'], shell: false });
    let stdout = '', stderr = '', settled = false;
    const closed = new Promise(done => child.once('close', (code, sig) => done({ code, signal: sig })));
    const stop = async () => {
      if (child.exitCode !== null || child.signalCode !== null) return { ...(await closed), already_exited: true };
      child.kill('SIGTERM');
      const result = await Promise.race([closed, new Promise(done => setTimeout(() => done(null), 5000))]);
      if (result) return result;
      child.kill('SIGKILL');
      return { ...(await closed), killed: true };
    };
    const fail = async error => { if (settled) return; settled = true; clearTimeout(timer); await stop().catch(() => {}); reject(error); };
    const timer = setTimeout(() => fail(new Error(`Memory hog was not ready within ${timeoutMs} ms.`)), timeoutMs);
    signal?.addEventListener('abort', () => fail(signal.reason ?? new Error('Cancelled')), { once: true });
    child.once('error', fail);
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
    child.stdout.on('data', chunk => {
      stdout += chunk;
      const line = stdout.split('\n').slice(0, -1).find(text => text.startsWith('{'));
      if (!line || settled) return;
      let ready; try { ready = JSON.parse(line); } catch { return; }
      if (ready.ready !== true) return;
      settled = true; clearTimeout(timer);
      resolve({ pid: child.pid, ready, stop });
    });
    closed.then(({ code, signal: sig }) => { if (!settled) fail(new Error(`Memory hog exited before ready (code ${code}, signal ${sig}): ${stderr.trim()}`)); });
  });
}

async function main() {
  const options = parseHogArgs(process.argv.slice(2));
  if (!options.force) {
    const decision = hogDecision({ gb: options.gb, availableBytes: (await sampleHost()).available_bytes });
    if (!decision.allowed) { console.error(JSON.stringify({ refused: true, ...decision })); process.exitCode = 3; return; }
  }
  const started = Date.now();
  const chunks = allocateTouched(Math.ceil(options.gb * GIB));
  console.log(JSON.stringify({ ready: true, pid: process.pid, bytes: chunks.reduce((sum, chunk) => sum + chunk.length, 0), allocate_ms: Date.now() - started }));
  const hold = setInterval(() => { if (!chunks.length) clearInterval(hold); }, 60_000);
  for (const sig of ['SIGTERM', 'SIGINT']) process.once(sig, () => { clearInterval(hold); chunks.length = 0; process.exit(0); });
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(error => { console.error(error.message); process.exitCode = 1; });
