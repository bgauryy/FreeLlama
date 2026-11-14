import { constants } from 'node:fs';
import { access, stat } from 'node:fs/promises';
import path from 'node:path';
import { loopbackOrigin } from './preflight-http.mjs';
import { preflightProtocol } from './preflight-models.mjs';

const nonempty = value => typeof value === 'string' && value.trim().length > 0;
const digest = value => typeof value === 'string' && /^(?:sha256:)?[a-f0-9]{64}$/.test(value) ? value.replace(/^sha256:/, '') : null;

export function validatePreflightConfig(config) {
  if (!config || typeof config !== 'object' || !config.model || !nonempty(config.model.ollama_tag) || !Array.isArray(config.vendors)) throw Error('Preflight config requires model.ollama_tag and a vendors array');
  if (config.model.artifact_sha256 !== undefined && !digest(config.model.artifact_sha256)) throw Error('model.artifact_sha256 must be a SHA-256 digest');
  for (const key of ['context_tokens', 'output_tokens']) if (config.model[key] !== undefined && (!Number.isSafeInteger(config.model[key]) || config.model[key] < 1)) throw Error(`model.${key} must be a positive integer`);
  const ids = new Set();
  for (const vendor of config.vendors) {
    if (!vendor || typeof vendor.id !== 'string' || !/^[a-z0-9][a-z0-9_-]*$/.test(vendor.id) || ids.has(vendor.id)) throw Error('Vendor IDs must be unique and filename-safe');
    ids.add(vendor.id);
    preflightProtocol(vendor.protocol);
    loopbackOrigin(vendor.base_url);
    if (vendor.enabled !== undefined && typeof vendor.enabled !== 'boolean') throw Error(`enabled must be boolean for ${vendor.id}`);
    if (vendor.model !== undefined && !nonempty(vendor.model)) throw Error(`model must be nonempty for ${vendor.id}`);
  }
  return config;
}

// A vendor with `start` is not running yet: the runner starts it, so its endpoint is not probed.
// Only the start command is checked: it must resolve (PATH or start.cwd) to an executable file.
export async function checkStartCommand(start, { env = process.env, cwd = process.cwd(), statImpl = stat, accessImpl = access } = {}) {
  const command = start?.command;
  if (!nonempty(command)) return { command: command ?? null, path: null, exists: false, executable: false, error: 'start.command is missing' };
  const searchPath = start.env?.PATH ?? env.PATH ?? '';
  const candidates = command.includes('/') ? [path.resolve(start.cwd ?? cwd, command)] : searchPath.split(path.delimiter).filter(Boolean).map(dir => path.join(dir, command));
  let found = null;
  for (const candidate of candidates) {
    try { if (!(await statImpl(candidate)).isFile()) continue; } catch { continue; }
    found ??= candidate;
    try { await accessImpl(candidate, constants.X_OK); return { command, path: candidate, exists: true, executable: true, error: null }; } catch { /* Try the next PATH entry. */ }
  }
  return { command, path: found, exists: found !== null, executable: false, error: found ? 'start command is not executable' : 'start command was not found' };
}
export function evaluateOwnedStart(startCheck) {
  return { readiness_kind: 'owned_start', status: 'owned_start', endpoint_checked: false, start_command: startCheck, ready: startCheck?.executable === true,
    note: 'Started by the runner; endpoint, model identity and admission are checked at run time.' };
}

const MODEL_LAYER = 'application/vnd.ollama.image.model', TENSOR_LAYER = 'application/vnd.ollama.image.tensor';
// Identity is the manifest's weight layer digest checked against the hashed blob. Ollama 0.40 tag
// digests differ from the manifest file hash, so the tag digest is information only.
export function weightIdentity(selected) {
  const layers = Array.isArray(selected?.layers) ? selected.layers : [];
  const models = layers.filter(layer => layer?.media_type === MODEL_LAYER);
  const weights = models.length ? models : layers.filter(layer => layer?.media_type === TENSOR_LAYER);
  const complete = selected?.complete === true && weights.length > 0 && weights.every(layer => layer.complete === true && digest(layer.digest));
  const hashed = weights.length > 0 && weights.every(layer => layer.hash_verified === true);
  const mismatch = weights.some(layer => layer.hash_verified === false);
  return { weight_digests: weights.map(layer => digest(layer.digest)).filter(Boolean), weight_hash_verified: hashed, ok: complete && !mismatch,
    status: !complete || mismatch ? 'unverified_or_mismatched' : hashed ? 'manifest_weight_layer_hash_verified' : 'manifest_weight_layer_present_unhashed' };
}

// HTTP success alone is not model/capacity readiness. Malformed/unknown evidence stays false/null.
export function evaluateVendorPrerequisites({ vendor, modelConfig, selected, health, readiness, models, route }) {
  const protocol = preflightProtocol(vendor.protocol), requested = vendor.model ?? modelConfig.ollama_tag;
  const entries = protocol.ollama ? models?.json?.models : health?.json?.data;
  const modelListValid = Array.isArray(entries) && (protocol.ollama ? models?.ok === true : health?.ok === true);
  const matches = modelListValid ? entries.filter(entry => entry && (protocol.ollama ? entry.name === requested || entry.model === requested : entry.id === requested)) : [];
  const observed = matches.length === 1 ? matches[0] : null;
  const observedDigest = digest(observed?.digest), localDigest = digest(selected?.manifest_sha256);
  const weight = weightIdentity(selected);
  // The tag digest is information only, but a malformed one is still malformed evidence.
  const tagDigestWellFormed = observed?.digest === undefined || observedDigest !== null;
  const identityMatches = protocol.ollama ? weight.ok && tagDigestWellFormed : null;
  const healthValid = health?.ok === true && (protocol.ollama ? nonempty(health.json?.version) : modelListValid);
  const readinessValid = protocol.freellama ? readiness?.ok === true && readiness.json?.ready === true && readiness.json?.ollama?.reachable === true : null;
  const assessment = route?.json?.execution?.resource_assessment;
  const admissibleStatuses = ['ready', 'ready_low_memory_estimate', 'ready_telemetry_unknown'];
  const admissible = protocol.freellama ? !!(route?.ok && assessment?.admissible === true && admissibleStatuses.includes(assessment.status)) : null;
  const capacityQualified = protocol.freellama ? admissible && assessment.status === 'ready' : null;
  const dispatchReady = protocol.freellama ? route?.json?.execution?.agent_plan?.dispatch_readiness === 'runnable_now' : null;
  return {
    health_payload_valid: healthValid,
    model_list_valid: modelListValid,
    requested_model: requested,
    requested_model_listed: matches.length > 0,
    requested_model_unique: matches.length === 1,
    local_weight_identity_ok: identityMatches,
    model_identity_status: protocol.ollama ? weight.status : 'unverified_openai_listing',
    local_weight_digests: protocol.ollama ? weight.weight_digests : null,
    tag_digest_info: protocol.ollama ? { live_tag_digest: observedDigest, local_manifest_file_sha256: localDigest, equal: observedDigest && localDigest ? observedDigest === localDigest : null, note: 'Information only; Ollama 0.40 tag digests differ from the manifest file hash.' } : null,
    observed_model: observed,
    readiness_payload_valid: readinessValid,
    admission_admissible_snapshot: admissible,
    admission_telemetry_qualified: capacityQualified,
    admission_dispatch_ready: dispatchReady,
    admission_ready_now: protocol.freellama ? !!(capacityQualified && dispatchReady) : null,
    ready: !!(healthValid && modelListValid && matches.length === 1 && (protocol.ollama ? identityMatches : true) && (!protocol.freellama || readinessValid && capacityQualified && dispatchReady)),
  };
}
