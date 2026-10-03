import { createServer, request, type Server } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createApp } from "../src/server/app";
import { parseConfig } from "../src/server/config";

const servers: Server[] = [];
const folders: string[] = [];
afterEach(async () => {
  await Promise.all(
    servers.splice(0).map(
      (server) =>
        new Promise<void>((resolve) => {
          server.close(() => resolve());
          server.closeAllConnections();
        }),
    ),
  );
  await Promise.all(
    folders
      .splice(0)
      .map((folder) => rm(folder, { recursive: true, force: true })),
  );
});
async function fixture() {
  const collector = { snapshot: vi.fn(async () => ({ schema_version: 1 })) };
  const folder = await mkdtemp(join(tmpdir(), "freellama-view-"));
  folders.push(folder);
  await writeFile(join(folder, "index.html"), "<h1>Runtime</h1>");
  const server = createServer(
    createApp({ collector: collector as never, staticDir: folder }),
  );
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("No address");
  return { collector, url: `http://127.0.0.1:${address.port}` };
}

describe("local view server", () => {
  it("serves UI and snapshot on one origin, with no mutation or arbitrary proxy route", async () => {
    const { collector, url } = await fixture();
    expect(await (await fetch(url)).text()).toContain("Runtime");
    const snapshot = await fetch(`${url}/api/view/snapshot`);
    expect(await snapshot.json()).toEqual({ schema_version: 1 });
    expect(snapshot.headers.get("cache-control")).toBe("no-store");
    expect(snapshot.headers.get("content-security-policy")).toContain(
      "frame-ancestors 'none'",
    );
    expect(
      (await fetch(`${url}/api/view/snapshot`, { method: "POST" })).status,
    ).toBe(405);
    expect(
      (await fetch(`${url}/api/view/snapshot?url=http://evil.test`)).status,
    ).toBe(400);
    expect((await fetch(`${url}/api/config/reload`)).status).toBe(404);
    expect((await fetch(`${url}/api/tasks`)).status).toBe(404);
    expect(collector.snapshot).toHaveBeenCalledTimes(1);
  });

  it("rejects cross-origin requests and DNS rebinding hosts before reading the control API", async () => {
    const { collector, url } = await fixture();
    const cases: Record<string, string>[] = [
      { Origin: "https://evil.test" },
      { Host: "evil.test" },
      { "Sec-Fetch-Site": "cross-site" },
    ];
    for (const headers of cases) {
      const status = await new Promise<number | undefined>(
        (resolve, reject) => {
          request(`${url}/api/view/snapshot`, { headers }, (response) => {
            response.resume();
            resolve(response.statusCode);
          })
            .on("error", reject)
            .end();
        },
      );
      expect(status).toBe(403);
    }
    expect(collector.snapshot).not.toHaveBeenCalled();
  });

  it("does not serve files outside the built client or dotfiles", async () => {
    const { url } = await fixture();
    expect((await fetch(`${url}/..%2fpackage.json`)).status).toBe(404);
    expect((await fetch(`${url}/.env`)).status).toBe(404);
    expect((await fetch(`${url}/missing.js`)).status).toBe(404);
  });
});

describe("configuration", () => {
  it("uses loopback defaults and rejects remote, credentialed, or ambiguous upstreams", () => {
    expect(parseConfig({})).toEqual({
      endpoint: "http://127.0.0.1:11435",
      port: 5173,
    });
    for (const endpoint of [
      "https://remote.test",
      "http://127.0.0.1.evil.test",
      "http://user:password@localhost",
      "http://localhost/api",
      "file:///etc/passwd",
      "http://localhost?token=secret",
    ]) {
      expect(() =>
        parseConfig({ FREELLAMA_SERVE_ENDPOINT: endpoint }),
      ).toThrow();
    }
    expect(
      parseConfig({ FREELLAMA_SERVE_ENDPOINT: "http://[::1]:11435" }).endpoint,
    ).toBe("http://[::1]:11435");
    for (const port of ["0", "65536", "NaN", "1.5", "", "1e3"])
      expect(() => parseConfig({ FREELLAMA_VIEW_PORT: port })).toThrow();
  });
});
