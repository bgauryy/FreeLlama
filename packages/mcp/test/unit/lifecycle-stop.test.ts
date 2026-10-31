import { createServer, type Server } from "node:http";
import { afterEach, describe, expect, it, vi } from "vitest";
import { stopModel } from "../../src/tools/ollama-lifecycle.js";

const servers: Server[] = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))));
});

/** A stand-in Ollama that records the unload request. */
async function ollama(): Promise<{ endpoint: string; bodies: unknown[] }> {
  const bodies: unknown[] = [];
  const server = createServer((request, response) => {
    let text = "";
    request.on("data", (chunk) => { text += chunk; });
    request.on("end", () => {
      bodies.push(JSON.parse(text));
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ model: "m", response: "", done: true, done_reason: "unload" }));
    });
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("no port");
  return { endpoint: `http://127.0.0.1:${address.port}`, bodies };
}

describe("ollama_manage stop", () => {
  it("unloads through serve when it answers, without asking Ollama", async () => {
    const served = vi.fn(async () => ({ model: "m", backend: "gpu", unloaded: true, status: "unloaded", unload_ms: 120 }));
    expect(await stopModel("m", undefined, undefined, served)).toEqual({
      model: "m", backend: "gpu", unloaded: true, status: "unloaded", unload_ms: 120, via: "freellama_serve",
    });
    expect(served).toHaveBeenCalledWith("m", { signal: undefined });
  });

  it("asks Ollama directly when serve is unreachable or too old", async () => {
    const { endpoint, bodies } = await ollama();
    vi.stubEnv("FREELLAMA_OLLAMA_ENDPOINT", endpoint);
    vi.resetModules();
    const { stopModel: stop } = await import("../../src/tools/ollama-lifecycle.js");
    const result = await stop("m", undefined, undefined, async () => undefined);
    expect(result).toMatchObject({ model: "m", unloaded: true, status: "unload_requested", via: "ollama" });
    expect(bodies).toEqual([{ model: "m", keep_alive: 0 }]);
    vi.unstubAllEnvs();
  });

  it("goes straight to a caller-named Ollama endpoint, and passes serve refusals through", async () => {
    const { endpoint, bodies } = await ollama();
    const served = vi.fn(async () => ({ unloaded: true }));
    expect(await stopModel("m", endpoint, undefined, served)).toMatchObject({ via: "ollama" });
    expect(served).not.toHaveBeenCalled();
    expect(bodies).toHaveLength(1);
    const refusal = JSON.stringify({ error: "tasks still running", code: "unload_wait_exceeded" });
    await expect(stopModel("m", undefined, undefined, async () => { throw new Error(refusal); })).rejects.toThrow(refusal);
    expect(bodies).toHaveLength(1);
  });
});

describe("serveUnload", () => {
  async function serve(status: number, body: string) {
    const seen: { auth?: string; body: unknown }[] = [];
    const server = createServer((request, response) => {
      let text = "";
      request.on("data", (chunk) => { text += chunk; });
      request.on("end", () => {
        seen.push({ auth: request.headers.authorization, body: request.url === "/_freellama/v1/unload" ? JSON.parse(text) : request.url });
        response.statusCode = status;
        response.end(body);
      });
    });
    servers.push(server);
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("no port");
    vi.stubEnv("FREELLAMA_SERVE_ENDPOINT", `http://127.0.0.1:${address.port}`);
    vi.stubEnv("FREELLAMA_MCP_AUTOSTART_SERVE", "0");
    vi.resetModules();
    const client = await import("../../src/serve-http.js");
    return { seen, unload: client.serveUnload };
  }
  afterEach(() => vi.unstubAllEnvs());

  it("returns serve's receipt and sends the bearer token", async () => {
    const { mkdtemp, writeFile } = await import("node:fs/promises");
    const { tmpdir } = await import("node:os");
    const path = await import("node:path");
    const folder = await mkdtemp(path.join(tmpdir(), "token-"));
    await writeFile(path.join(folder, "token"), `${"k".repeat(40)}\n`);
    vi.stubEnv("FREELLAMA_AUTH_TOKEN_FILE", path.join(folder, "token"));
    const { seen, unload } = await serve(200, JSON.stringify({ model: "m", unloaded: true, status: "unloaded" }));
    expect(await unload("m", { maxWaitSeconds: 5 })).toEqual({ model: "m", unloaded: true, status: "unloaded" });
    expect(seen).toEqual([{ auth: `Bearer ${"k".repeat(40)}`, body: { model: "m", max_wait_seconds: 5 } }]);
  });

  it("reports an older serve (404) as absent and throws other refusals with the receipt", async () => {
    expect(await (await serve(404, "")).unload("m")).toBeUndefined();
    const refusal = JSON.stringify({ error: "tasks still running", code: "unload_wait_exceeded" });
    await expect((await serve(503, refusal)).unload("m")).rejects.toThrow(refusal);
  });

  it("reports an unreachable serve as absent", async () => {
    vi.stubEnv("FREELLAMA_SERVE_ENDPOINT", "http://127.0.0.1:9");
    vi.stubEnv("FREELLAMA_MCP_AUTOSTART_SERVE", "0");
    vi.resetModules();
    const { serveUnload } = await import("../../src/serve-http.js");
    expect(await serveUnload("m")).toBeUndefined();
  });
});
