import http from "node:http";
import https from "node:https";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({ route: vi.fn(), warm: vi.fn(), live: vi.fn() }));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: vi.fn(), getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: (...args: unknown[]) => fixture.route(...args),
  createSession: vi.fn(), deleteSession: vi.fn(), killSession: vi.fn(), listModels: vi.fn(), warmModelRequest: (...args: unknown[]) => fixture.warm(...args),
  listTaskJobs: vi.fn(), createScope: vi.fn(), getScope: vi.fn(), forkScope: vi.fn(), deleteScope: vi.fn(),
}));
vi.mock("../../src/model-library.js", async (original) => ({
  ...(await original<Record<string, unknown>>()), catalogLiveState: (...args: unknown[]) => fixture.live(...args),
}));

import { startGateway } from "../../src/agent-cli/gateway.js";
import { streamRun, streamTarget } from "../../src/agent-cli/stream.js";
import { managedShiftOff, residentContext } from "../../src/agent-cli/virtual.js";

const token = "fixture-operator-token-for-authenticated-serve";
const directory = mkdtempSync(join(tmpdir(), "freellama-cli-auth-"));
const tokenFile = join(directory, "token");
writeFileSync(tokenFile, `${token}\n`, { mode: 0o600 });
afterAll(() => rmSync(directory, { recursive: true, force: true }));
const render = { json: false, verbose: false, quiet: false, hideThinking: false };
const io = () => ({ out: vi.fn(), err: vi.fn(), write: vi.fn() });

describe("agent CLI authenticated serve boundary", () => {
  let server: http.Server;
  let endpoint: string;
  let gateway: Awaited<ReturnType<typeof startGateway>> | undefined;
  let redirect: string | undefined;
  let contextConfig: unknown;
  let customReply: { type: string; body: string; status?: number; hang?: boolean } | undefined;
  let upstreamResponse: http.ServerResponse | undefined;
  let upstreamClosed = false;
  const requests: Array<{ path: string; authorization?: string }> = [];

  beforeEach(async () => {
    vi.stubEnv("FREELLAMA_AUTH_TOKEN_FILE", tokenFile);
    vi.stubEnv("FREELLAMA_MCP_AUTOSTART_SERVE", "0");
    fixture.route.mockResolvedValue(JSON.stringify({ selected_model: "fixture:1b", options: { num_ctx: 4096 } }));
    fixture.warm.mockResolvedValue(JSON.stringify({ status: "warmed" }));
    fixture.live.mockResolvedValue([[{ name: "fixture:1b", capabilities: ["completion"] }], 64e9]);
    requests.length = 0;
    redirect = undefined;
    contextConfig = { context_shift: "on" };
    customReply = undefined;
    upstreamResponse = undefined;
    upstreamClosed = false;
    server = http.createServer((request, response) => {
      request.resume();
      request.on("end", () => {
        const path = request.url ?? "/";
        requests.push({ path, authorization: request.headers.authorization });
        if (request.headers.authorization !== `Bearer ${token}`) return void response.writeHead(401).end('{"error":"unauthorized"}');
        response.setHeader("content-type", "application/json");
        if (path === "/api/ps") return void response.end(JSON.stringify({ models: [{ name: "fixture:1b", context_length: 4096 }] }));
        if (path === "/_freellama/v1/config") return void response.end(JSON.stringify(contextConfig));
        if (path === "/_freellama/v1/routes") return void response.end(JSON.stringify({ task_evidence: "none" }));
        if (path === "/api/tags") return void response.end(JSON.stringify({ models: [{ name: "fixture:1b" }] }));
        if (path === "/v1/models") return void response.end(JSON.stringify({ data: [{ id: "fixture:1b" }] }));
        if (customReply && ["/api/chat", "/v1/chat/completions"].includes(path)) {
          upstreamResponse = response;
          response.on("close", () => { upstreamClosed = true; });
          response.statusCode = customReply.status ?? 200;
          response.setHeader("content-type", customReply.type);
          response.write(customReply.body);
          if (!customReply.hang) response.end();
          return;
        }
        if (path === "/api/chat") {
          if (redirect) return void response.writeHead(302, { location: redirect }).end();
          response.setHeader("content-type", "application/x-ndjson");
          return void response.end('{"message":{"role":"assistant","content":"hello"},"done":true}\n');
        }
        if (path === "/v1/chat/completions") {
          response.setHeader("content-type", "text/event-stream");
          return void response.end('data: {"model":"fixture:1b","choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n');
        }
        response.end(JSON.stringify({ version: "fixture" }));
      });
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    endpoint = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
  });

  afterEach(async () => {
    await gateway?.close();
    gateway = undefined;
    await new Promise<void>((resolve) => server.close(() => resolve()));
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it("authenticates residency/config observations and terminal generation", async () => {
    expect(await residentContext("fixture:1b", endpoint)).toBe(4096);
    expect(await managedShiftOff(endpoint)).toBe(false);
    const capture = io();
    const args = { model: "fixture:1b", prompt: "hello", endpoint };
    expect(await streamRun(streamTarget(args)!, args, render, capture, new AbortController().signal, false)).toBe(0);
    expect(capture.write.mock.calls.flat().join("")).toBe("hello\n");
    expect(requests.find((request) => request.path === "/api/chat")?.authorization).toBe(`Bearer ${token}`);
  });

  it("authenticates model lists, real-tag passthrough and virtual SSE despite SDK placeholder auth", async () => {
    gateway = await startGateway({ port: 0, serveEndpoint: endpoint });
    for (const path of ["/api/tags", "/v1/models"]) {
      const response = await fetch(`${gateway.url}${path}`);
      const value = await response.json();
      expect(JSON.stringify(value)).toContain("fixture:1b");
    }
    const raw = await fetch(`${gateway.url}/api/version`, { headers: { authorization: "Bearer sdk-placeholder" } });
    expect(raw.status).toBe(200);
    const streamed = await fetch(`${gateway.url}/v1/chat/completions`, {
      method: "POST", headers: { "content-type": "application/json", authorization: "Bearer sdk-placeholder" },
      body: JSON.stringify({ model: "freellama/auto", messages: [{ role: "user", content: "hello" }], stream: true }),
    });
    expect(streamed.status).toBe(200);
    expect(await streamed.text()).toContain("hello");
    expect(requests.every((request) => request.authorization === `Bearer ${token}`)).toBe(true);
  });

  it("reads value/source config receipts and refreshes a changed context-shift policy", async () => {
    contextConfig = { settings: { context_shift: { value: "ollama_default", source: "file" } } };
    expect(await managedShiftOff(endpoint)).toBe(false);
    contextConfig = { settings: { context_shift: { value: "off", source: "file" } } };
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 6000);
    expect(await managedShiftOff(endpoint)).toBe(true);
  });

  it("keeps absolute-form proxy URLs on the configured serve origin", async () => {
    let foreignRequests = 0;
    const foreign = http.createServer((request, response) => { foreignRequests++; request.resume(); response.end("foreign"); });
    await new Promise<void>((resolve) => foreign.listen(0, "127.0.0.1", resolve));
    try {
      gateway = await startGateway({ port: 0, serveEndpoint: endpoint });
      const foreignUrl = `http://127.0.0.1:${(foreign.address() as { port: number }).port}/api/version`;
      for (const path of [foreignUrl, `http://gateway//127.0.0.1:${(foreign.address() as { port: number }).port}/api/version`]) {
        const status = await new Promise<number>((resolve, reject) => {
          const request = http.request(gateway!.url, { path }, (response) => { response.resume(); response.on("end", () => resolve(response.statusCode!)); });
          request.on("error", reject); request.end();
        });
        expect(status).toBe(200);
      }
      expect(foreignRequests).toBe(0);
      expect(requests.every((request) => request.authorization === `Bearer ${token}`)).toBe(true);
    } finally {
      await new Promise<void>((resolve) => foreign.close(() => resolve()));
    }
  });

  it("does not follow a generation redirect to another origin", async () => {
    let foreignRequests = 0;
    const foreign = http.createServer((request, response) => { foreignRequests++; request.resume(); response.end('{"done":true,"message":{"content":"foreign"}}\n'); });
    await new Promise<void>((resolve) => foreign.listen(0, "127.0.0.1", resolve));
    try {
      redirect = `http://127.0.0.1:${(foreign.address() as { port: number }).port}/api/chat`;
      const args = { model: "fixture:1b", prompt: "hello", endpoint };
      expect(await streamRun(streamTarget(args)!, args, render, io(), new AbortController().signal, false)).toBe(1);
      expect(foreignRequests).toBe(0);
    } finally {
      await new Promise<void>((resolve) => foreign.close(() => resolve()));
    }
  });

  it("uses TLS for an HTTPS serve and rejects its untrusted certificate before sending HTTP credentials", async () => {
    let httpRequests = 0;
    const tls = https.createServer({
      key: readFileSync(new URL("../fixtures/tls/server.key", import.meta.url)),
      cert: readFileSync(new URL("../fixtures/tls/server.crt", import.meta.url)),
    }, (request, response) => { httpRequests++; request.resume(); response.end("unexpected trusted request"); });
    tls.on("tlsClientError", () => undefined);
    await new Promise<void>((resolve) => tls.listen(0, "127.0.0.1", resolve));
    try {
      const tlsEndpoint = `https://127.0.0.1:${(tls.address() as { port: number }).port}`;
      gateway = await startGateway({ port: 0, serveEndpoint: tlsEndpoint });
      const reply = await fetch(`${gateway.url}/api/version`);
      expect(reply.status).toBe(502);
      expect((await reply.json()).error).toMatch(/self.signed|certificate|unable to verify/i);
      expect(httpRequests).toBe(0);
    } finally {
      await new Promise<void>((resolve) => tls.close(() => resolve()));
    }
  });

  it.each([200, 503])("cancels oversized terminal content or error bodies without a newline (HTTP %s)", async (status) => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    customReply = { type: status === 200 ? "application/x-ndjson" : "application/json", body: "x".repeat(257), hang: true, status };
    const args = { model: "fixture:1b", prompt: "hello", endpoint };
    const capture = io();
    expect(await streamRun(streamTarget(args)!, args, render, capture, new AbortController().signal, false)).toBe(1);
    expect(capture.err.mock.calls.flat().join(" ")).toMatch(/over 256 wire bytes/);
    await vi.waitFor(() => expect(upstreamClosed).toBe(true));
    expect(requests.filter((request) => request.path === "/api/chat")).toHaveLength(1);
  });

  it("reports overflow as failure after partial terminal output has already been printed", async () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    customReply = { type: "application/x-ndjson", body: '{"message":{"content":"partial"},"done":false}\n', hang: true };
    const args = { model: "fixture:1b", prompt: "hello", endpoint };
    const capture = io();
    const running = streamRun(streamTarget(args)!, args, render, capture, new AbortController().signal, false);
    await vi.waitFor(() => expect(capture.write).toHaveBeenCalledWith("partial"));
    upstreamResponse!.write("x".repeat(257));
    expect(await running).toBe(1);
    expect(capture.err.mock.calls.flat().join(" ")).toContain("partial output is incomplete");
    await vi.waitFor(() => expect(upstreamClosed).toBe(true));
  });

  it("accepts a terminal response at exactly its UTF-8 wire-byte limit", async () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    const value = '{"message":{"content":"exacté"},"done":true}\n';
    customReply = { type: "application/x-ndjson", body: value + " ".repeat(256 - Buffer.byteLength(value)) };
    const args = { model: "fixture:1b", prompt: "hello", endpoint };
    const capture = io();
    expect(await streamRun(streamTarget(args)!, args, render, capture, new AbortController().signal, false)).toBe(0);
    expect(capture.write.mock.calls.flat().join("")).toBe("exacté\n");
  });

  it.each(["application/x-ndjson", "text/event-stream", "application/json"])("cancels oversized virtual gateway %s responses", async (type) => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    customReply = { type, body: "x".repeat(257), hang: true };
    gateway = await startGateway({ port: 0, serveEndpoint: endpoint });
    const path = type === "application/x-ndjson" ? "/api/chat" : "/v1/chat/completions";
    const read = async () => {
      const reply = await fetch(`${gateway!.url}${path}`, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: "freellama/auto", messages: [{ role: "user", content: "hello" }], stream: true }),
      });
      if (type === "application/json") {
        expect(reply.status).toBe(502);
        expect((await reply.json()).error).toMatch(/over 256 wire bytes/);
      } else await reply.text();
    };
    if (type === "application/json") await read();
    else await expect(read()).rejects.toThrow();
    await vi.waitFor(() => expect(upstreamClosed).toBe(true));
    expect(requests.filter((request) => request.path === path)).toHaveLength(1);
  });

  it("accepts a buffered virtual JSON response at the exact wire limit", async () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    const value = '{"model":"fixture:1b","choices":[{"message":{"content":"exact"}}]}';
    customReply = { type: "application/json", body: value.padEnd(256) };
    gateway = await startGateway({ port: 0, serveEndpoint: endpoint });
    const reply = await fetch(`${gateway.url}/v1/chat/completions`, {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "freellama/auto", messages: [{ role: "user", content: "hello" }], stream: true }),
    });
    expect(reply.status).toBe(200);
    expect((await reply.json()).choices[0].message.content).toBe("exact");
  });

  it("keeps real-tag passthrough pull-driven and byte-preserving above the rewrite budget", async () => {
    vi.stubEnv("FREELLAMA_MAX_REQUEST_BYTES", "256");
    customReply = { type: "application/x-ndjson", body: "x".repeat(257) };
    gateway = await startGateway({ port: 0, serveEndpoint: endpoint });
    const reply = await fetch(`${gateway.url}/api/chat`, {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "fixture:1b", messages: [{ role: "user", content: "hello" }], stream: true }),
    });
    expect(reply.status).toBe(200);
    expect(await reply.text()).toBe(customReply.body);
  });
});
