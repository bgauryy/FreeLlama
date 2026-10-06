// HTTP client for the research adapters' chat and catalog requests. Failures are classified for
// retry decisions: a non-2xx status is an `HTTPError`, a failure before the request is fully sent is
// a `URLError` (nothing reached the server), and an idle socket after the request was sent is a
// `RequestTimeoutError` (inference may still be running upstream).
import { readFileSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import { ValidationError } from "./errors.js";
import { cpLength, decodeUtf8, isSpace, trimSpace } from "./text.js";

export const RECEIPT_BODY_LIMIT = 64 * 1024;

/** A non-2xx response; `body` holds up to {@link RECEIPT_BODY_LIMIT} + 1 response bytes. */
export class HTTPError extends Error {
  override name = "HTTPError";
  constructor(readonly code: number, readonly reason: string, readonly body: Buffer = Buffer.alloc(0)) {
    super(`HTTP Error ${code}: ${reason}`);
  }
}

/** The request could not be sent; `reason` is the underlying error (a Node system error or timeout). */
export class URLError extends Error {
  override name = "URLError";
  constructor(readonly reason: Error) {
    super(`request not sent: ${reason.message}`);
  }
}

/** The request was sent, then the socket stayed idle past the timeout. */
export class RequestTimeoutError extends Error {
  override name = "RequestTimeoutError";
  constructor(message = "timed out") {
    super(message);
  }
}

/** Build transport headers, reading a bearer token from a file rather than process args. */
export function requestHeaders(env: Record<string, string | undefined> = process.env): Record<string, string> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  const tokenFile = trimSpace(env.FREELLAMA_AUTH_TOKEN_FILE ?? "");
  if (!tokenFile) return headers;
  const token = trimSpace(decodeUtf8(readFileSync(tokenFile), "FREELLAMA_AUTH_TOKEN_FILE"));
  if (cpLength(token) < 32 || [...token].some((character) => isSpace(character))) {
    throw new ValidationError("FREELLAMA_AUTH_TOKEN_FILE must contain one token of at least 32 bytes");
  }
  headers.authorization = `Bearer ${token}`;
  return headers;
}

export interface HttpOptions {
  method: "GET" | "POST";
  headers: Record<string, string>;
  body?: Buffer;
  timeoutSeconds: number;
  /** Read at most this many response bytes; unlimited by default. */
  limit?: number;
}

/**
 * Send one request and read the response body. A non-2xx status rejects with {@link HTTPError}. A
 * failure before the request is fully sent rejects with {@link URLError}; a socket failure after
 * that rejects with the system error itself, and an idle socket past the timeout rejects with
 * {@link RequestTimeoutError}.
 */
export function httpRequest(url: string, options: HttpOptions): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      reject(new ValidationError(`unknown url type: ${JSON.stringify(url)}`));
      return;
    }
    const client = parsed.protocol === "https:" ? https : parsed.protocol === "http:" ? http : null;
    if (client === null) {
      reject(new URLError(new Error(`unknown url type: ${parsed.protocol.replace(/:$/, "")}`)));
      return;
    }
    const headers: Record<string, string> = { ...options.headers, connection: "close" };
    if (options.body) headers["content-length"] = String(options.body.length);
    let sent = false;
    let settled = false;
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      request.destroy();
      reject(error);
    };
    const request = client.request(parsed, { method: options.method, headers });
    request.setTimeout(Math.max(options.timeoutSeconds * 1000, 1), () => {
      fail(sent ? new RequestTimeoutError() : new URLError(new RequestTimeoutError()));
    });
    request.on("finish", () => {
      sent = true;
    });
    request.on("error", (error: Error) => fail(sent ? error : new URLError(error)));
    request.on("response", (response) => {
      const chunks: Buffer[] = [];
      let size = 0;
      const status = response.statusCode ?? 0;
      const ok = status >= 200 && status < 300;
      const limit = ok ? options.limit : RECEIPT_BODY_LIMIT + 1;
      const finish = () => {
        if (settled) return;
        settled = true;
        const body = Buffer.concat(chunks);
        request.destroy();
        if (ok) resolve(body);
        else reject(new HTTPError(status, response.statusMessage ?? "", body));
      };
      response.on("data", (chunk: Buffer) => {
        if (limit !== undefined && size >= limit) return;
        const piece = limit !== undefined ? chunk.subarray(0, limit - size) : chunk;
        chunks.push(piece);
        size += piece.length;
        if (limit !== undefined && size >= limit) finish();
      });
      response.on("error", (error: Error) => fail(error));
      response.on("end", finish);
    });
    request.end(options.body);
  });
}

/** Decode a JSON response body as strict UTF-8. */
export function parseJsonBody(body: Buffer, source: string): unknown {
  return JSON.parse(decodeUtf8(body, source));
}

/** POST a JSON payload and decode the JSON response. */
export async function requestJson(url: string, payload: unknown, timeoutSeconds: number): Promise<unknown> {
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  const response = await httpRequest(url, { method: "POST", headers: requestHeaders(), body, timeoutSeconds });
  return parseJsonBody(response, "chat response");
}
