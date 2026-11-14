import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { classifyModelFiles, preflightProtocol } from './preflight-models.mjs';
import { checkStartCommand, evaluateOwnedStart, evaluateVendorPrerequisites, validatePreflightConfig } from './preflight-checks.mjs';

test('preflight verifies weight bytes and rejects mismatched declarations and empty manifests without inference', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'freellama-preflight-'));
  try {
    const weights = Buffer.from('GGUF');
    const sha = createHash('sha256').update(weights).digest('hex');
    const models = path.join(dir, 'models');
    await mkdir(path.join(models, 'blobs'), { recursive: true });
    await mkdir(path.join(models, 'manifests/registry.ollama.ai/library/fixture'), { recursive: true });
    await writeFile(path.join(models, 'blobs', `sha256-${sha}`), weights);
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/good'), JSON.stringify({ layers: [{ mediaType: 'application/vnd.ollama.image.model', digest: `sha256:${sha}`, size: 4 }] }));
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/empty'), JSON.stringify({ layers: [] }));
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/tensor'), JSON.stringify({ format: 'safetensors', runner: 'mlx', layers: [{ mediaType: 'application/vnd.ollama.image.tensor', digest: `sha256:${sha}`, size: 4 }] }));
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/missing-tensor'), JSON.stringify({ format: 'safetensors', runner: 'mlx', layers: [{ mediaType: 'application/vnd.ollama.image.tensor', digest: `sha256:${'f'.repeat(64)}`, size: 4 }] }));
    const emptySha = createHash('sha256').update('').digest('hex');
    await writeFile(path.join(models, 'blobs', `sha256-${emptySha}`), '');
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/zero-weight'), JSON.stringify({ layers: [{ mediaType: 'application/vnd.ollama.image.tensor', digest: `sha256:${emptySha}`, size: 0 }] }));
    const directoryBlob = path.join(models, 'blobs', `sha256-${'d'.repeat(64)}`);
    await mkdir(directoryBlob);
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/directory-weight'), JSON.stringify({ layers: [{ mediaType: 'application/vnd.ollama.image.tensor', digest: `sha256:${'d'.repeat(64)}`, size: (await stat(directoryBlob)).size }] }));
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/null-descriptor'), JSON.stringify({ layers: [{ mediaType: 'application/vnd.ollama.image.model', digest: `sha256:${sha}`, size: 4 }, null] }));
    const config = path.join(dir, 'config.json');
    await writeFile(config, JSON.stringify({ model: { ollama_tag: 'fixture:good', artifact_sha256: '0'.repeat(64) }, vendors: [] }));
    const out = path.join(dir, 'out');
    try { execFileSync(process.execPath, [fileURLToPath(new URL('./preflight.mjs', import.meta.url)), '--config', config, '--models-dir', models, '--out', out, '--hash-model'], { timeout: 30000, stdio: 'pipe' }); assert.fail('Mismatched declaration must block readiness'); }
    catch (e) { assert.equal(e.status, 2); }
    const report = JSON.parse(await readFile(path.join(out, 'preflight.json'), 'utf8'));
    assert.equal(report.artifact_identity.observed_sha256, sha);
    assert.equal(report.artifact_identity.hash_verified, true);
    assert.ok(report.blockers.includes('Declared model artifact does not match the selected local weight digest'));
    assert.equal(report.model_inventory.find(m => m.tag === 'fixture:empty').complete, false);
    const tensor = report.model_inventory.find(m => m.tag === 'fixture:tensor');
    assert.equal(tensor.files_complete, true);
    assert.equal(tensor.complete, true);
    assert.equal(tensor.weight_payload_kind, 'tensor_shards');
    assert.equal(tensor.common_gguf_eligible, false);
    assert.equal(report.model_inventory.find(m => m.tag === 'fixture:missing-tensor').complete, false);
    assert.equal(report.model_inventory.find(m => m.tag === 'fixture:good').common_gguf_eligible, true);
    for (const tag of ['fixture:zero-weight', 'fixture:directory-weight', 'fixture:null-descriptor']) assert.equal(report.model_inventory.find(m => m.tag === tag).complete, false);
    assert.ok(report.blockers.includes('No enabled benchmark vendors are configured'));
    assert.equal(report.measured_benchmark, false);
    assert.deepEqual(report.model_downloads_performed, []);
    assert.deepEqual(report.vendors, []);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('complete metadata alone is not a model, and malformed weight formats are not GGUF-eligible', () => {
  assert.deepEqual(classifyModelFiles([]), { files_complete: false, weight_payload_kind: 'none', complete: false, common_gguf_eligible: false });
  const metadataOnly = classifyModelFiles([{ media_type: 'application/vnd.ollama.image.json', complete: true }]);
  assert.equal(metadataOnly.files_complete, true);
  assert.equal(metadataOnly.complete, false);
  const wrongMagic = classifyModelFiles([{ media_type: 'application/vnd.ollama.image.model', complete: true, magic: 'DATA' }]);
  assert.equal(wrongMagic.complete, true);
  assert.equal(wrongMagic.common_gguf_eligible, false);
});

test('managed FreeLlama uses the same local health and routing prerequisites as raw FreeLlama', () => {
  assert.deepEqual(preflightProtocol('freellama-managed'), preflightProtocol('freellama'));
  assert.deepEqual(preflightProtocol('freellama-managed'), { freellama: true, ollama: true });
  assert.deepEqual(preflightProtocol('openai'), { freellama: false, ollama: false });
  assert.deepEqual(preflightProtocol('freellama-stream'), preflightProtocol('freellama-managed'));
});

const fixturePrerequisites = () => ({
  vendor: { id: 'managed', protocol: 'freellama-managed', model: 'fixture:good' },
  modelConfig: { ollama_tag: 'fixture:good' },
  selected: { complete: true, manifest_sha256: 'a'.repeat(64), layers: [{ media_type: 'application/vnd.ollama.image.model', digest: `sha256:${'c'.repeat(64)}`, complete: true, hash_verified: true }] },
  health: { ok: true, json: { version: 'fixture-version' } },
  readiness: { ok: true, json: { ready: true, ollama: { reachable: true } } },
  models: { ok: true, json: { models: [{ name: 'fixture:good', digest: 'a'.repeat(64) }] } },
  route: { ok: true, json: { execution: { resource_assessment: { status: 'ready', admissible: true }, agent_plan: { dispatch_readiness: 'runnable_now' } } } },
});

test('identity uses the manifest weight layer and the tag digest is information only', () => {
  const inputs = fixturePrerequisites();
  let result = evaluateVendorPrerequisites(inputs);
  assert.equal(result.ready, true);
  assert.equal(result.model_identity_status, 'manifest_weight_layer_hash_verified');
  assert.deepEqual(result.local_weight_digests, ['c'.repeat(64)]);
  assert.equal(result.tag_digest_info.equal, true);
  // Ollama 0.40: the tag digest differs from the manifest file hash, and that must not block.
  inputs.models.json.models[0].digest = 'b'.repeat(64);
  result = evaluateVendorPrerequisites(inputs);
  assert.equal(result.ready, true);
  assert.equal(result.tag_digest_info.equal, false);
  assert.equal(result.tag_digest_info.live_tag_digest, 'b'.repeat(64));
  delete inputs.selected.layers[0].hash_verified;
  assert.equal(evaluateVendorPrerequisites(inputs).model_identity_status, 'manifest_weight_layer_present_unhashed');
  inputs.selected.layers[0].hash_verified = false;
  assert.equal(evaluateVendorPrerequisites(inputs).ready, false);
  assert.equal(evaluateVendorPrerequisites(inputs).local_weight_identity_ok, false);
  inputs.selected.layers = [{ media_type: 'application/vnd.ollama.image.json', digest: `sha256:${'c'.repeat(64)}`, complete: true }];
  assert.equal(evaluateVendorPrerequisites(inputs).ready, false);
});

test('HTTP 200 cannot make malformed, ambiguous or explicitly unready evidence ready', () => {
  for (const mutate of [
    inputs => { inputs.health.json = {}; },
    inputs => { inputs.readiness.json.ready = false; },
    inputs => { inputs.models.json.models = {}; },
    inputs => { inputs.models.ok = false; },
    inputs => { inputs.models.json.models[0].digest = {}; },
    inputs => { inputs.models.json.models.push({ ...inputs.models.json.models[0] }); },
    inputs => { inputs.route.json.execution.resource_assessment.admissible = false; },
    inputs => { inputs.route.json.execution.resource_assessment.status = 'ready_unrecognized'; },
    inputs => { inputs.route.json.execution.agent_plan.dispatch_readiness = 'queue_likely'; },
  ]) {
    const inputs = fixturePrerequisites(); mutate(inputs);
    assert.equal(evaluateVendorPrerequisites(inputs).ready, false);
  }
});

test('unknown/advisory runtime admissibility stays explicit and cannot qualify a clean baseline', () => {
  for (const status of ['ready_telemetry_unknown', 'ready_low_memory_estimate']) {
    const inputs = fixturePrerequisites();
    inputs.route.json.execution.resource_assessment.status = status;
    const result = evaluateVendorPrerequisites(inputs);
    assert.equal(result.admission_admissible_snapshot, true);
    assert.equal(result.admission_telemetry_qualified, false);
    assert.equal(result.ready, false);
  }
});

test('OpenAI model listing does not claim observed file identity or resource telemetry', () => {
  const result = evaluateVendorPrerequisites({ ...fixturePrerequisites(), vendor: { protocol: 'openai', model: 'fixture:good' }, health: { ok: true, json: { data: [{ id: 'fixture:good' }] } } });
  assert.equal(result.ready, true);
  assert.equal(result.local_weight_identity_ok, null);
  assert.equal(result.tag_digest_info, null);
  assert.equal(result.admission_telemetry_qualified, null);
  assert.equal(result.model_identity_status, 'unverified_openai_listing');
});

test('preflight rejects malformed configurations before any environment probes', () => {
  const good = () => ({ model: { ollama_tag: 'fixture:good' }, vendors: [{ id: 'one', protocol: 'ollama', base_url: 'http://localhost:11434' }] });
  for (const mutate of [
    config => { config.vendors[0].protocol = 'typo'; },
    config => { config.vendors[0].base_url += '/wrong'; },
    config => { config.vendors.push({ ...config.vendors[0] }); },
    config => { config.vendors[0].enabled = 'false'; },
    config => { config.model.context_tokens = -1; },
    config => { config.model.artifact_sha256 = 'not-a-digest'; },
    config => { config.vendors[0].id = 1; },
  ]) {
    const config = good(); mutate(config);
    assert.throws(() => validatePreflightConfig(config));
  }
});

test('start command resolution: PATH or start.cwd, executable files only', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'freellama-preflight-start-'));
  try {
    const bin = path.join(dir, 'bin');
    await mkdir(bin);
    await writeFile(path.join(bin, 'plain'), 'not executable', { mode: 0o644 });
    await writeFile(path.join(bin, 'server'), '#!/bin/sh\n', { mode: 0o755 });
    const onPath = await checkStartCommand({ command: 'server', args: [] }, { env: { PATH: bin } });
    assert.deepEqual([onPath.exists, onPath.executable, onPath.path], [true, true, path.join(bin, 'server')]);
    const relative = await checkStartCommand({ command: './bin/server', args: [], cwd: dir }, { env: { PATH: '' } });
    assert.equal(relative.executable, true);
    const plain = await checkStartCommand({ command: 'plain', args: [] }, { env: { PATH: bin } });
    assert.deepEqual([plain.exists, plain.executable, plain.error], [true, false, 'start command is not executable']);
    const missing = await checkStartCommand({ command: 'nope-not-here', args: [], env: { PATH: bin } }, { env: { PATH: '/usr/bin' } });
    assert.deepEqual([missing.exists, missing.error], [false, 'start command was not found']);
    assert.equal((await checkStartCommand({ args: [] })).error, 'start.command is missing');
    assert.equal(evaluateOwnedStart(onPath).ready, true);
    assert.equal(evaluateOwnedStart(onPath).status, 'owned_start');
    assert.equal(evaluateOwnedStart(plain).ready, false);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('preflight reports start vendors as owned_start without probing their endpoint', async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'freellama-preflight-owned-'));
  try {
    const weights = Buffer.from('GGUF');
    const sha = createHash('sha256').update(weights).digest('hex');
    const models = path.join(dir, 'models');
    await mkdir(path.join(models, 'blobs'), { recursive: true });
    await mkdir(path.join(models, 'manifests/registry.ollama.ai/library/fixture'), { recursive: true });
    await writeFile(path.join(models, 'blobs', `sha256-${sha}`), weights);
    await writeFile(path.join(models, 'manifests/registry.ollama.ai/library/fixture/good'), JSON.stringify({ layers: [{ mediaType: 'application/vnd.ollama.image.model', digest: `sha256:${sha}`, size: 4 }] }));
    const config = path.join(dir, 'config.json');
    // Port 9 (discard) is never probed: an owned_start vendor is checked by its command only.
    await writeFile(config, JSON.stringify({ model: { ollama_tag: 'fixture:good', artifact_sha256: sha }, vendors: [
      { id: 'owned', protocol: 'openai', base_url: 'http://127.0.0.1:9', model: 'bench-model', start: { command: process.execPath, args: [] } },
      { id: 'missing', protocol: 'openai', base_url: 'http://127.0.0.1:9', model: 'bench-model', start: { command: 'freellama-bench-no-such-command', args: [] } },
    ] }));
    const out = path.join(dir, 'out');
    try { execFileSync(process.execPath, [fileURLToPath(new URL('./preflight.mjs', import.meta.url)), '--config', config, '--models-dir', models, '--out', out, '--hash-model'], { timeout: 60000, stdio: 'pipe' }); assert.fail('A missing start command must block readiness'); }
    catch (e) { assert.equal(e.status, 2); }
    const report = JSON.parse(await readFile(path.join(out, 'preflight.json'), 'utf8'));
    const [owned, missing] = report.vendors;
    assert.deepEqual([owned.status, owned.ready, owned.endpoint_checked, owned.start_command.path], ['owned_start', true, false, process.execPath]);
    assert.equal(Object.hasOwn(owned, 'health'), false);
    assert.deepEqual([missing.status, missing.ready], ['owned_start', false]);
    assert.deepEqual(report.blockers, ['missing: start command was not found: freellama-bench-no-such-command']);
    assert.match(await readFile(path.join(out, 'preflight.md'), 'utf8'), /\| owned \| yes \| owned_start \(start command executable; endpoint checked by the runner\) \|/);
  } finally { await rm(dir, { recursive: true, force: true }); }
});
