import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

// A WAV header plus one second of 24 kHz 16-bit silence, as Ollama's speech endpoint returns.
const WAV = Buffer.concat([Buffer.from("RIFF0000WAVEfmt "), Buffer.alloc(28), Buffer.alloc(48_000)]);

describe("audio through serve's passthrough", () => {
  let server: Server;
  let endpoint: string;
  let client: Client;
  const requests: { path: string; type: string; body: Buffer }[] = [];
  beforeAll(async () => {
    server = createServer(async (request, response) => {
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(chunk as Buffer);
      requests.push({ path: request.url!, type: request.headers["content-type"] ?? "", body: Buffer.concat(chunks) });
      if (request.url === "/v1/audio/speech") {
        response.setHeader("content-type", "audio/wav");
        response.end(WAV);
        return;
      }
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ text: "the build failed on line 12" }));
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing fixture address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient({ FREELLAMA_MCP_AUTOSTART_SERVE: "0" });
  });
  beforeEach(() => { requests.length = 0; });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });
  const call = (args: Record<string, unknown>) => client.callTool({ name: "audio", arguments: { endpoint, ...args } });

  it("speaks to a WAV file and returns only its path and length", async () => {
    const outputFile = path.join(mkdtempSync(path.join(tmpdir(), "freellama-audio-")), "reply.wav");
    const result = await call({ action: "speak", model: "phonon:en", text: "Tests passed.", voice: "Freya", outputFile });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(result.structuredContent).toMatchObject({ file: outputFile, bytes: WAV.length, seconds: 1, model: "phonon:en" });
    expect(readFileSync(outputFile)).toEqual(WAV);
    expect(JSON.parse(requests.find((request) => request.path === "/v1/audio/speech")!.body.toString())).toEqual({ model: "phonon:en", input: "Tests passed.", voice: "Freya", response_format: "wav" });
  });

  it("transcribes a WAV named by path as a multipart upload", async () => {
    const file = path.join(mkdtempSync(path.join(tmpdir(), "freellama-audio-")), "note.wav");
    writeFileSync(file, WAV);
    const result = await call({ action: "transcribe", model: "lfm3-audio:2b", file, language: "en" });
    expect(result.isError, JSON.stringify(result)).not.toBe(true);
    expect(result.structuredContent).toMatchObject({ text: "the build failed on line 12", model: "lfm3-audio:2b" });
    expect((result.content as Array<{ text: string }>)[0]!.text).toBe("[lfm3-audio:2b] the build failed on line 12");
    const request = requests.find((request) => request.path === "/v1/audio/transcriptions")!;
    expect(request.type).toMatch(/^multipart\/form-data/);
    const form = request.body.toString("latin1");
    expect(form).toContain('name="model"\r\n\r\nlfm3-audio:2b');
    expect(form).toContain('name="language"\r\n\r\nen');
    expect(form).toContain('filename="note.wav"');
  });

  it("refuses fields of another action and files outside the readable roots before any request", async () => {
    for (const [args, message] of [
      [{ action: "speak", model: "m", text: "t", file: "x.wav" }, /`file` does not apply to action/],
      [{ action: "transcribe", model: "m", file: "x.wav", voice: "v" }, /`voice` does not apply/],
      [{ action: "transcribe", model: "m" }, /requires `file`/],
      [{ action: "transcribe", model: "m", file: "/etc/hosts" }, /outside the readable roots/],
    ] as const) {
      const result = await call(args);
      expect(result.isError).toBe(true);
      expect(JSON.stringify(result.content)).toMatch(message);
    }
    expect(requests).toHaveLength(0);
  });
});

describe("audio refusals", () => {
  it("shrink serve's admission receipt to one actionable line", async () => {
    const { refusalLine } = await import("../../src/helpers.js");
    const body = JSON.stringify({ error: "host resource admission deadline exceeded", resource_admission: { assessment: {
      held_by: ["active_swapping"], effective_available_bytes: 4_283_826_176, required_with_reserve_bytes: 3_340_966_486, status: "held_host_pressure",
    }, snapshot: { observation: { big: "x".repeat(3000) } } } });
    const line = refusalLine(body, 503);
    expect(line).toBe('host resource admission deadline exceeded (held by active_swapping): 4.3 GB available, 3.3 GB needed with reserve; retry once the host frees memory (doctor{view:"status"} shows what holds it)');
    expect(refusalLine("plain failure", 500)).toBe("plain failure");
  });

  it("keeps a core refusal that already states its memory figures as is", async () => {
    const { refusalLine } = await import("../../src/helpers.js");
    const error = "host resource admission deadline exceeded: needs 15.6 GB incl. 5.2 GB reserve, 4.2 GB free";
    const body = JSON.stringify({ error, code: "resource_admission_unavailable", resource_admission: { assessment: {
      effective_available_bytes: 4_200_000_000, required_with_reserve_bytes: 15_600_000_000,
    } } });
    expect(refusalLine(body, 503)).toBe(`${error} [resource_admission_unavailable]`);
  });
});
