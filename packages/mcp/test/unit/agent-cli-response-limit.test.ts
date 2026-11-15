import { afterEach, describe, expect, it, vi } from "vitest";
import { boundedResponseBytes, boundedResponseText, responseByteLimit, ResponseLimitError } from "../../src/agent-cli/response-limit.js";
import { sseWithReceipt, withChecksOnDone } from "../../src/agent-cli/gateway.js";

afterEach(() => vi.unstubAllEnvs());

describe("agent response wire-byte budget", () => {
  it.each([undefined, "", "0", "-1", "1.5", "12bytes", "9007199254740992"])("uses the shared 64MiB default for invalid config %s", (value) => {
    if (value === undefined) vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", undefined);
    else vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", value);
    expect(responseByteLimit()).toBe(64 * 1024 * 1024);
  });

  it("accepts a positive configured byte limit", () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", " +42 ");
    expect(responseByteLimit()).toBe(42);
  });

  it("counts UTF-8 wire bytes and accepts the exact limit across chunks", async () => {
    async function* source() { yield Buffer.from("é"); yield Buffer.from("a"); }
    const chunks = [];
    for await (const chunk of boundedResponseBytes(source(), 3)) chunks.push(chunk);
    expect(Buffer.concat(chunks).toString()).toBe("éa");
  });

  it("cancels an open fetch body before an oversized incomplete record can be accumulated", async () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "3");
    const cancel = vi.fn();
    const body = new ReadableStream<Uint8Array>({
      start(controller) { controller.enqueue(Buffer.from("ab")); controller.enqueue(Buffer.from("cd")); }, cancel,
    });
    await expect(boundedResponseText(new Response(body))).rejects.toBeInstanceOf(ResponseLimitError);
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it.each(["ndjson", "sse"])("bounds %s without requiring a newline and closes the input iterator", async (kind) => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "5");
    let closed = false;
    async function* source() { try { yield Buffer.from("abcdef"); } finally { closed = true; } }
    const resolution = { model: "fixture:1b", options: {}, task: "completion" };
    const transform = kind === "ndjson" ? withChecksOnDone("/api/chat", "freellama/auto", resolution, undefined, false)
      : sseWithReceipt("/v1/chat/completions", "freellama/auto", resolution, undefined);
    const consume = async () => { for await (const _ of transform(source())) { /* consume */ } };
    await expect(consume()).rejects.toBeInstanceOf(ResponseLimitError);
    expect(closed).toBe(true);
  });

  it.each(["ndjson", "sse"])("accepts exactly the complete %s response wire size", async (kind) => {
    const wire = kind === "ndjson" ? '{"message":{"content":"é"},"done":true}\n'
      : 'data: {"model":"fixture:1b","choices":[{"delta":{"content":"é"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n';
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", String(Buffer.byteLength(wire)));
    async function* source() { yield Buffer.from(wire); }
    const resolution = { model: "fixture:1b", options: {}, task: "completion" };
    const transform = kind === "ndjson" ? withChecksOnDone("/api/chat", "freellama/auto", resolution, undefined, false)
      : sseWithReceipt("/v1/chat/completions", "freellama/auto", resolution, undefined);
    let output = "";
    for await (const chunk of transform(source())) output += chunk;
    expect(output).toContain("é");
    expect(output).toContain("freellama");
  });
});
