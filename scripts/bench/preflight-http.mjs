// Read-only loopback health and decision-preview requests. No service lifecycle actions.
export function loopbackOrigin(base) {
  const url = new URL(base);
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname) || !['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
    throw Error('Only local loopback health endpoints without credentials are allowed');
  }
  if (url.pathname !== '/' || url.search || url.hash) throw Error('Health base URL must be an origin without a path, query or fragment');
  return url.origin;
}

async function boundedText(response, limitBytes) {
  if (!response.body) return '';
  const reader = response.body.getReader(), decoder = new TextDecoder();
  let bytes = 0, text = '';
  try {
    for (;;) {
      const next = await reader.read();
      if (next.done) return text + decoder.decode();
      bytes += next.value.byteLength;
      if (bytes > limitBytes) throw Error(`Health response exceeds ${limitBytes} bytes`);
      text += decoder.decode(next.value, { stream: true });
    }
  } catch (error) { await reader.cancel().catch(() => {}); throw error; }
  finally { reader.releaseLock(); }
}

export async function probeLoopback(base, route, body, { fetchImpl = fetch, timeoutMs = 3000, maxResponseBytes = 4 * 1024 * 1024 } = {}) {
  try {
    const origin = loopbackOrigin(base);
    if (typeof route !== 'string' || !route.startsWith('/') || route.startsWith('//')) throw Error('Health route must be an absolute local path');
    const hasBody = body !== undefined;
    const response = await fetchImpl(`${origin}${route}`, {
      method: hasBody ? 'POST' : 'GET',
      headers: hasBody ? { 'content-type': 'application/json' } : undefined,
      body: hasBody ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(timeoutMs),
      redirect: 'error',
    });
    const responseText = await boundedText(response, maxResponseBytes);
    let json = null, parsed = false;
    try { json = JSON.parse(responseText); parsed = true; } catch {}
    return { ok: response.ok, status: response.status, json, text: parsed ? undefined : responseText.slice(0, 2000) };
  } catch (error) {
    return { ok: false, error: `${error.message}${error.cause?.code ? ` (${error.cause.code})` : ''}` };
  }
}
