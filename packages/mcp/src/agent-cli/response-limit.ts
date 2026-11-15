// Bound bytes before decoding or accumulating response text, including incomplete records.
const DEFAULT_RESPONSE_BYTES = 64 * 1024 * 1024;

export function responseByteLimit(): number {
  const value = process.env.FREELLAMA_MAX_REQUEST_BYTES?.trim();
  if (!value || !/^\+?\d+$/.test(value)) return DEFAULT_RESPONSE_BYTES;
  const bytes = Number(value);
  return Number.isSafeInteger(bytes) && bytes > 0 ? bytes : DEFAULT_RESPONSE_BYTES;
}

export class ResponseLimitError extends Error {
  constructor(limit: number) {
    super(`response is over ${limit} wire bytes (FREELLAMA_MAX_REQUEST_BYTES); partial output is incomplete`);
    this.name = "ResponseLimitError";
  }
}

/** Closing this iterator also closes its source: fetch readers cancel, and Node streams destroy. */
export async function* boundedResponseBytes(source: AsyncIterable<Uint8Array>, limit = responseByteLimit()): AsyncGenerator<Uint8Array> {
  let used = 0;
  for await (const bytes of source) {
    if (bytes.byteLength > limit - used) throw new ResponseLimitError(limit);
    used += bytes.byteLength;
    yield bytes;
  }
}

export async function boundedResponseText(response: Response): Promise<string> {
  if (!response.body) return "";
  const decoder = new TextDecoder();
  let text = "";
  for await (const bytes of boundedResponseBytes(response.body as unknown as AsyncIterable<Uint8Array>)) {
    text += decoder.decode(bytes, { stream: true });
  }
  return text + decoder.decode();
}
