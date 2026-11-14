// File completeness is independent of eligibility for the shared GGUF benchmark.
export function classifyModelFiles(layers) {
  const entries = Array.isArray(layers) ? layers : [];
  const models = entries.filter(layer => layer?.media_type === 'application/vnd.ollama.image.model');
  const tensors = entries.filter(layer => layer?.media_type === 'application/vnd.ollama.image.tensor');
  const filesComplete = entries.length > 0 && entries.every(layer => layer?.complete === true);
  const weightPayloadKind = models.length ? 'model_file' : tensors.length ? 'tensor_shards' : 'none';
  const complete = filesComplete && weightPayloadKind !== 'none';
  return {
    files_complete: filesComplete,
    weight_payload_kind: weightPayloadKind,
    complete,
    // This identifies file format only; backend support and fit still require validation.
    common_gguf_eligible: complete && models.length === 1 && models[0].magic === 'GGUF',
  };
}

export function preflightProtocol(protocol) {
  if (!['ollama', 'openai', 'freellama', 'freellama-managed', 'freellama-stream'].includes(protocol)) throw Error(`Unsupported preflight protocol: ${protocol}`);
  const freellama = protocol !== 'ollama' && protocol !== 'openai';
  return { freellama, ollama: freellama || protocol === 'ollama' };
}
