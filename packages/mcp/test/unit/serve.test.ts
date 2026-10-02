import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";

let folder: string | undefined;
let serve: typeof import("../../src/serve.js") | undefined;
const listeners: Server[] = [];
afterEach(async () => {
  serve?.stopAutostartedServe();
  await Promise.all(listeners.splice(0).map((listener) => new Promise<void>((resolve) => listener.close(() => resolve()))));
  if (folder) await rm(folder, { recursive: true, force: true });
  vi.unstubAllEnvs();
  vi.resetModules();
});

async function fixture() {
  const reserve = createServer();
  listeners.push(reserve);
  await new Promise<void>((resolve) => reserve.listen(0, "127.0.0.1", resolve));
  const address = reserve.address();
  if (!address || typeof address === "string") throw new Error("No test port");
  await new Promise<void>((resolve) => reserve.close(() => resolve()));
  const endpoint = `http://127.0.0.1:${address.port}`;
  folder = await mkdtemp(join(tmpdir(), "freellama-startup-"));
  const binary = join(folder, "freellama");
  await writeFile(binary, "Not executable\n", { mode: 0o600 });
  vi.stubEnv("FREELLAMA_SERVE_BINARY", binary);
  vi.stubEnv("FREELLAMA_SERVE_ENDPOINT", endpoint);
  vi.stubEnv("FREELLAMA_MCP_AUTOSTART_SERVE", "1");
  vi.resetModules();
  serve = await import("../../src/serve.js");
  return { binary, endpoint, port: address.port };
}

it.skipIf(process.platform === "win32")("reports an unexecutable binary without killing the caller and permits retry", async () => {
  const { binary, endpoint } = await fixture();
  const failed = [serve!.ensureServe(undefined), serve!.ensureServe(undefined)];
  for (const startup of failed) await expect(startup).rejects.toThrow(/Could not start freellama serve.*EACCES/);
  await writeFile(binary, `#!${process.execPath}\nimport('node:http').then(({createServer}) => {
    const args = process.argv.slice(2); const listen = args[args.indexOf('--listen') + 1];
    createServer((req,res) => { res.end('{}'); }).listen(Number(listen.split(':').at(-1)), '127.0.0.1');
  });\n`);
  await chmod(binary, 0o700);
  await expect(serve!.ensureServe(undefined)).resolves.toBeUndefined();
  expect((await fetch(`${endpoint}/_freellama/v1/health`)).status).toBe(200);
}, 10_000);

it.skipIf(process.platform === "win32")("keeps a new child tracked when an older stopped child exits", async () => {
  const { binary, endpoint } = await fixture();
  await writeFile(binary, `#!${process.execPath}\nimport('node:http').then(({createServer}) => {
    const args = process.argv.slice(2); const listen = args[args.indexOf('--listen') + 1];
    const server = createServer((req,res) => { res.end('{}'); });
    server.listen(Number(listen.split(':').at(-1)), '127.0.0.1');
    process.on('SIGTERM', () => server.close(() => setTimeout(() => process.exit(0), 600)));
  });\n`);
  await chmod(binary, 0o700);
  await serve!.ensureServe(undefined);
  serve!.stopAutostartedServe();
  await new Promise((resolve) => setTimeout(resolve, 100));
  await serve!.ensureServe(undefined);
  await new Promise((resolve) => setTimeout(resolve, 700));
  serve!.stopAutostartedServe();
  await new Promise((resolve) => setTimeout(resolve, 100));
  await expect(fetch(`${endpoint}/_freellama/v1/health`)).rejects.toThrow();
}, 10_000);
