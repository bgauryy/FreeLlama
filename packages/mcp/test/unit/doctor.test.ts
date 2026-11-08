import { createServer, type Server } from "node:http";
import { afterEach, describe, expect, it, vi } from "vitest";

// The compiled addon is not loadable under vitest; these helpers do not use it.
vi.mock("../../src/native.js", () => ({}));
import { doctorStatus, ollamaProblems, serveReadiness } from "../../src/tools/doctor.js";

const servers: Server[] = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))));
});

async function serve(status: number, body: string): Promise<string> {
  const server = createServer((_request, response) => {
    response.statusCode = status;
    response.end(body);
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("no port");
  return `http://127.0.0.1:${address.port}`;
}

describe("doctor status", () => {
  it("is ok only when serve is live and ready", () => {
    expect(doctorStatus({ platform_health: { status: "ok" }, readiness: { ready: true } })).toEqual({ status: "ok", problems: [] });
    // An older serve without /ready is judged by liveness alone.
    expect(doctorStatus({ platform_health: { status: "ok" }, readiness: { supported: false } }).status).toBe("ok");
    expect(doctorStatus({ platform_health_unavailable: "freellama serve unreachable: refused" })).toEqual({
      status: "degraded", problems: ["freellama serve unreachable: refused"],
    });
    expect(doctorStatus({ platform_health: { status: "ok" }, readiness: { ready: false, waiting_for: "ollama", detail: "connection refused" } }))
      .toEqual({ status: "degraded", problems: ["freellama serve not ready: waiting for ollama (connection refused)"] });
  });

  it("reads /ready as serve reports it", async () => {
    const ready = { ready: true, ollama: { reachable: true, version: "0.12" } };
    expect(await serveReadiness(await serve(200, JSON.stringify(ready)))).toEqual(ready);
    const waiting = { ready: false, waiting_for: "catalog", detail: "first fetch" };
    expect(await serveReadiness(await serve(503, JSON.stringify(waiting)))).toEqual(waiting);
    expect(await serveReadiness(await serve(404, ""))).toMatchObject({ supported: false });
    expect(await serveReadiness(await serve(401, "{}"))).toEqual({ http_status: 401 });
    expect(await serveReadiness("http://127.0.0.1:9")).toHaveProperty("unreachable");
  });
});

describe("doctor status problems", () => {
  it("names each backend whose Ollama residency query failed", () => {
    expect(ollamaProblems({ loaded_models: [{ backend: "gpu", error: "error sending request for url (http://127.0.0.1:11434/api/ps)" }] }))
      .toEqual([expect.stringMatching(/^Ollama unreachable on backend gpu .*ask the user to start Ollama$/)]);
    expect(ollamaProblems({ loaded_models: [{ backend: "gpu", models: [] }] })).toEqual([]);
    expect(ollamaProblems({})).toEqual([]);
  });
});
