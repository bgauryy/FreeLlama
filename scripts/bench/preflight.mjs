#!/usr/bin/env node
// Read-only prerequisite inspection. Never starts a service, pulls or unloads a model.
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { access, mkdir, open, readFile, readdir, stat, statfs, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs, promisify } from 'node:util';
import { sampleHost } from './host.mjs';
import { probeLoopback as http } from './preflight-http.mjs';
import { classifyModelFiles, preflightProtocol } from './preflight-models.mjs';
import { checkStartCommand, evaluateOwnedStart, evaluateVendorPrerequisites, validatePreflightConfig } from './preflight-checks.mjs';

const exec = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const { values } = parseArgs({ options: { config: { type: 'string', default: 'scripts/bench/vendors.example.json' }, out: { type: 'string' }, 'models-dir': { type: 'string' }, 'hash-model': { type: 'boolean' }, help: { type: 'boolean' } } });
if (values.help) { console.log('Usage: node scripts/bench/preflight.mjs [--config FILE] [--out NEW_DIRECTORY] [--models-dir DIR] [--hash-model]\nInspects local files and local HTTP health only. Writes preflight.json and preflight.md. Exit 2 means prerequisites are blocked.'); process.exit(0); }
const configPath = path.resolve(values.config);
const configText = await readFile(configPath, 'utf8');
const config = validatePreflightConfig(JSON.parse(configText));
const out = path.resolve(values.out ?? `.octocode/bench/preflight-${new Date().toISOString().replace(/[:.]/g, '-')}`);
await mkdir(path.dirname(out), { recursive: true });
await mkdir(out); // Never overwrite evidence from an earlier check.
async function run(command, args = []) {
  try { const r = await exec(command, args, { cwd: root, timeout: 15000, maxBuffer: 1024 * 1024 }); return { ok: true, stdout: r.stdout.trim(), stderr: r.stderr.trim() }; }
  catch (e) { return { ok: false, stdout: e.stdout?.trim() ?? '', error: e.stderr?.trim() || e.message }; }
}
async function locate(name) {
  for (const dir of (process.env.PATH ?? '').split(path.delimiter)) { const candidate = path.join(dir, name); try { await access(candidate, 1); return candidate; } catch {} }
  return null;
}
async function walk(dir) {
  const files = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) { const p = path.join(dir, entry.name); if (entry.isDirectory()) files.push(...await walk(p)); else if (entry.isFile()) files.push(p); }
  return files;
}
const modelRoot = path.resolve(values['models-dir'] ?? process.env.OLLAMA_MODELS ?? path.join(os.homedir(), '.ollama/models'));
const inventory = [], inventoryErrors = [];
try {
  for (const file of (await walk(path.join(modelRoot, 'manifests'))).sort()) {
    try {
      const text = await readFile(file, 'utf8'); const manifest = JSON.parse(text);
      if (!manifest || typeof manifest !== 'object' || Array.isArray(manifest) || manifest.layers !== undefined && !Array.isArray(manifest.layers)) throw Error('Invalid model manifest structure');
      const parts = path.relative(path.join(modelRoot, 'manifests'), file).split(path.sep);
      const tag = `${parts.slice(0, -1).join('/')}:${parts.at(-1)}`.replace(/^registry\.ollama\.ai\/library\//, '');
      const layers = [];
      for (const layer of [...(manifest.config === undefined ? [] : [manifest.config]), ...(manifest.layers ?? [])]) {
        if (!layer || typeof layer !== 'object' || Array.isArray(layer)) { layers.push({ complete: false, error: 'Invalid layer descriptor' }); continue; }
        if (!/^sha256:[a-f0-9]{64}$/.test(layer.digest)) { layers.push({ media_type: layer.mediaType, complete: false, error: 'Invalid layer digest' }); continue; }
        const weight = ['application/vnd.ollama.image.model', 'application/vnd.ollama.image.tensor'].includes(layer.mediaType);
        if (!Number.isSafeInteger(layer.size) || layer.size < 0 || weight && layer.size === 0) { layers.push({ media_type: layer.mediaType, digest: layer.digest, complete: false, error: 'Invalid layer byte size' }); continue; }
        const blob = path.join(modelRoot, 'blobs', layer.digest.replace(':', '-'));
        let diskBytes = null, magic = null, fileError = null;
        try {
          const info = await stat(blob);
          if (!info.isFile()) throw Error('Layer blob is not a regular file');
          diskBytes = info.size;
          if (layer.mediaType === 'application/vnd.ollama.image.model') { const handle = await open(blob); try { const b = Buffer.alloc(4); const read = await handle.read(b, 0, 4, 0); magic = read.bytesRead === 4 ? b.toString('ascii') : null; } finally { await handle.close(); } }
        } catch (error) { fileError = error.message; }
        const item = { media_type: layer.mediaType, digest: layer.digest, expected_bytes: layer.size, disk_bytes: diskBytes, path: blob, complete: fileError === null && diskBytes === layer.size, magic, ...(fileError ? { error: fileError } : {}) };
        if (values['hash-model'] && tag === config.model.ollama_tag && layer.mediaType === 'application/vnd.ollama.image.model' && item.complete) {
          const h = createHash('sha256'); for await (const chunk of createReadStream(blob)) h.update(chunk);
          item.observed_sha256 = h.digest('hex'); item.hash_verified = item.observed_sha256 === layer.digest.slice(7);
          item.complete &&= item.hash_verified;
        }
        layers.push(item);
      }
      inventory.push({ tag, manifest_sha256: createHash('sha256').update(text).digest('hex'), format: manifest.format ?? null, runner: manifest.runner ?? null, ...classifyModelFiles(layers), layers });
    } catch (e) { inventoryErrors.push(`${file}: ${e.message}`); }
  }
} catch (e) { inventoryErrors.push(e.message); }
const tools = {};
for (const [name, args] of [['node', ['--version']], ['yarn', ['--version']], ['cargo', ['--version']], ['rustc', ['--version']], ['curl', ['--version']], ['jq', ['--version']], ['ollama', ['--version']], ['llama-server', ['--version']], ['llama-swap', ['--version']], ['local-ai', ['--version']], ['lemonade', ['--version']], ['lms', ['--version']], ['omlx', ['--version']], ['docker', ['--version']]]) {
  const executable = name === 'node' ? process.execPath : await locate(name); tools[name] = { path: executable, ...(executable ? await run(executable, args) : { ok: false, error: 'Not on PATH' }) };
}
tools.freellama = { path: path.join(root, 'target/release/freellama'), ...await run(path.join(root, 'target/release/freellama'), ['--version']) };
const vendors = [];
const selected = inventory.find(x => x.tag === config.model.ollama_tag);
const primaryWeight = selected?.layers.find(x => x.media_type === 'application/vnd.ollama.image.model');
const declaredArtifact = config.model.artifact_sha256?.replace(/^sha256:/, '');
for (const v of config.vendors) {
  const protocol = preflightProtocol(v.protocol);
  if (v.start) { vendors.push({ id: v.id, enabled: v.enabled !== false, base_url: v.base_url, protocol: v.protocol, ...evaluateOwnedStart(await checkStartCommand(v.start)) }); continue; }
  const requestedModel = v.model ?? config.model.ollama_tag;
  const healthRoute = protocol.ollama ? '/api/version' : '/v1/models';
  const health = await http(v.base_url, healthRoute);
  const readiness = protocol.freellama ? await http(v.base_url, '/_freellama/v1/ready') : null;
  const models = protocol.ollama && health.ok ? await http(v.base_url, '/api/tags') : null;
  const route = v.enabled !== false && protocol.freellama && readiness?.ok && readiness.json?.ready === true ? await http(v.base_url, '/_freellama/v1/routes', { task: 'completion', model: requestedModel, context_tokens: config.model.context_tokens ?? 4096 }) : null;
  vendors.push({ id: v.id, enabled: v.enabled !== false, base_url: v.base_url, protocol: v.protocol, health, readiness, routing_preview: route, ...evaluateVendorPrerequisites({ vendor: v, modelConfig: config.model, selected, health, readiness, models, route }) });
}
const model = inventory.find(x => x.tag === config.model.ollama_tag);
const blocks = [];
const requiredTools = ['node'];
for (const name of requiredTools) if (!tools[name].ok) blocks.push(`${name}: ${tools[name].error}`);
if (!config.vendors.some(vendor => vendor.enabled !== false)) blocks.push('No enabled benchmark vendors are configured');
if (!model) blocks.push(`Selected model ${config.model.ollama_tag} was not found in the inspected store; verify --models-dir against the running daemon before recommending any download`);
else if (!model.complete) blocks.push(`Selected model ${config.model.ollama_tag} has invalid, missing or incomplete files in the inspected store`);
if (declaredArtifact && primaryWeight?.digest !== `sha256:${declaredArtifact}`) blocks.push('Declared model artifact does not match the selected local weight digest');
for (const v of vendors.filter(x => x.enabled && !x.ready)) blocks.push(`${v.id}: ${v.readiness_kind === 'owned_start' ? `${v.start_command.error}: ${v.start_command.command}` : v.admission_admissible_snapshot === true && !v.admission_telemetry_qualified ? 'runtime permits this snapshot, but advisory/unknown resource evidence does not qualify a clean benchmark baseline' : v.admission_dispatch_ready === false && v.readiness_payload_valid ? 'snapshot does not permit immediate dispatch for a clean benchmark baseline' : 'endpoint, model identity or clean-baseline readiness evidence is missing or invalid'}`);
const disk = await statfs(root);
const report = { kind: 'prerequisite_check', readiness_scope: 'Enabled-vendor local-file/endpoint smoke readiness; clean FreeLlama baseline additionally requires a currently admissible, telemetry-qualified, immediately dispatchable decision-only preview. Advisory/unknown runtime-admissible states remain recorded separately. This is no inference validation, comparison qualification or capacity reservation.', measured_benchmark: false, comparison_qualified: false, required_tools: requiredTools, created_at: new Date().toISOString(), config_file: configPath, config_sha256: createHash('sha256').update(configText).digest('hex'), host: { hostname: os.hostname(), platform: process.platform, release: os.release(), arch: os.arch(), cpus: os.cpus().length, total_memory_bytes: os.totalmem(), disk_available_bytes: disk.bavail * disk.bsize, sample: await sampleHost() }, source: { revision: await run('git', ['rev-parse', 'HEAD']), changes: await run('git', ['status', '--short']) }, tools, models_root: modelRoot, model_inventory: inventory, inventory_errors: inventoryErrors, selected_model: model?.tag ?? null, artifact_identity: { declared_sha256: declaredArtifact ?? null, manifest_weight_sha256: primaryWeight?.digest?.slice(7) ?? null, observed_sha256: primaryWeight?.observed_sha256 ?? null, hash_verified: primaryWeight?.hash_verified ?? false }, vendors, blockers: blocks, ready: blocks.length === 0, model_downloads_performed: [], installations_performed: [] };
await writeFile(path.join(out, 'preflight.json'), JSON.stringify(report, null, 2) + '\n');
const clean = (s) => String(s ?? '').replaceAll('|', '\\|').replaceAll('\n', ' ');
const lines = ['# Benchmark prerequisite check', '', `Checked: ${report.created_at}`, '', '**No benchmark was run.** No models were downloaded or services stopped.', '', `Host: ${report.host.platform} ${report.host.arch}, ${report.host.cpus} logical CPUs, ${(report.host.total_memory_bytes / 1024 ** 3).toFixed(1)} GiB RAM, ${(report.host.disk_available_bytes / 1024 ** 3).toFixed(1)} GiB disk available.`, '', '| Vendor | Selected | Endpoint and model ready |', '|---|---|---|', ...vendors.map(v => `| ${v.id} | ${v.enabled ? 'yes' : 'no'} | ${v.readiness_kind === 'owned_start' ? `owned_start (${v.ready ? 'start command executable; endpoint checked by the runner' : clean(v.start_command.error)})` : v.ready ? 'yes' : 'no'} |`), '', ...vendors.filter(v => v.tag_digest_info?.live_tag_digest).map(v => `Info: ${v.id} tag digest ${v.tag_digest_info.live_tag_digest}; manifest file sha256 ${v.tag_digest_info.local_manifest_file_sha256 ?? 'unknown'}. Identity uses the manifest weight layer (${v.model_identity_status}).`), '', `Local manifests: ${inventory.length}; complete model files: ${inventory.filter(x => x.complete).length}; common GGUF file format: ${inventory.filter(x => x.common_gguf_eligible).length}. Tensor-only safetensors models can have complete files without qualifying for the common GGUF suite. File sizes establish local completeness; hashes are verified only for the selected weight file with --hash-model.`, '', '| Tool | Located | Version / diagnostic |', '|---|---|---|', ...Object.entries(tools).map(([name,v]) => `| ${name} | ${v.path ? 'yes' : 'no'} | ${clean(v.stdout || v.stderr || v.error)} |`), '', '## Blockers', '', ...blocks.map(x => `- ${clean(x)}`), '', 'See preflight.json for exact paths, digests, byte sizes, HTTP results and missing telemetry.'];
lines.splice(6, 0, report.readiness_scope, '', 'Only Node is required by the API harness. Other tool versions are inventory; source builds and release checks are separate requirements.', '');
await writeFile(path.join(out, 'preflight.md'), lines.join('\n') + '\n');
console.log(`${report.ready ? 'READY' : 'BLOCKED'}: ${path.join(out, 'preflight.md')}`);
process.exitCode = report.ready ? 0 : 2;
