import type { SpawnOptions } from "node:child_process";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";

const fixtureLaunch = vi.hoisted(() => ({ script: undefined as string | undefined }));
vi.mock("node:child_process", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:child_process")>();
  return {
    ...actual,
    spawn(command: string, args: string[], options: SpawnOptions) {
      // Launch generated fixture scripts through Node to avoid host loader delays for executable scripts.
      if (command === fixtureLaunch.script) return actual.spawn(process.execPath, [command, ...args], options);
      return actual.spawn(command, args, options);
    },
  };
});

let folder: string | undefined;
let serve: typeof import("../../src/serve.js") | undefined;
const listeners: Server[] = [];
afterEach(async () => {
  serve?.stopAutostartedServe();
  fixtureLaunch.script = undefined;
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
  fixtureLaunch.script = binary;
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
  fixtureLaunch.script = binary;
  await serve!.ensureServe(undefined);
  serve!.stopAutostartedServe();
  await new Promise((resolve) => setTimeout(resolve, 100));
  await serve!.ensureServe(undefined);
  await new Promise((resolve) => setTimeout(resolve, 700));
  serve!.stopAutostartedServe();
  await new Promise((resolve) => setTimeout(resolve, 100));
  await expect(fetch(`${endpoint}/_freellama/v1/health`)).rejects.toThrow();
}, 10_000);

it.skipIf(process.platform === "win32")("starts serve in the background at launch and logs a failure instead of rejecting", async () => {
  const { binary, endpoint } = await fixture();
  const stderr = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
  serve!.startServeInBackground();
  await vi.waitFor(() => expect(stderr).toHaveBeenCalledWith(expect.stringMatching(/background start failed: Could not start freellama serve.*EACCES/)), { timeout: 5_000 });
  stderr.mockRestore();
  await writeFile(binary, `#!${process.execPath}\nimport('node:http').then(({createServer}) => {
    const args = process.argv.slice(2); const listen = args[args.indexOf('--listen') + 1];
    createServer((req,res) => { res.end('{}'); }).listen(Number(listen.split(':').at(-1)), '127.0.0.1');
  });\n`);
  await chmod(binary, 0o700);
  fixtureLaunch.script = binary;
  serve!.startServeInBackground();
  await vi.waitFor(async () => expect((await fetch(`${endpoint}/_freellama/v1/health`)).status).toBe(200), { timeout: 5_000 });
}, 10_000);

/** A serve stand-in: records its arguments, answers /ready with `phases` in turn, then ready. */
function readyScript(argsFile: string, phases: { status: number; body: unknown }[]) {
  return `#!${process.execPath}\nimport('node:http').then(({createServer}) => {
    const args = process.argv.slice(2); const listen = args[args.indexOf('--listen') + 1];
    require('node:fs').writeFileSync(${JSON.stringify(argsFile)}, JSON.stringify(args));
    const phases = ${JSON.stringify(phases)}; let index = 0;
    process.stderr.write('store 3 ms\\n');
    createServer((req,res) => {
      if (req.url !== '/_freellama/v1/ready') { res.end('{}'); return; }
      const phase = phases[index++];
      if (!phase) { res.end(JSON.stringify({ ready: true })); return; }
      res.statusCode = phase.status; res.end(JSON.stringify(phase.body));
    }).listen(Number(listen.split(':').at(-1)), '127.0.0.1');
  });\n`;
}

it.skipIf(process.platform === "win32")("waits while the catalog loads, reports phases and passes the operator runtime file", async () => {
  const home = await mkdtemp(join(tmpdir(), "freellama-home-"));
  await writeFile(join(home, "runtime.toml"), "# operator settings\n");
  vi.stubEnv("FREELLAMA_HOME", home);
  vi.stubEnv("FREELLAMA_RUNTIME_CONFIG", "");
  const { binary, endpoint } = await fixture();
  const argsFile = join(home, "args.json");
  const catalog = { status: 503, body: { ready: false, waiting_for: "catalog", detail: "first fetch" } };
  await writeFile(binary, readyScript(argsFile, [catalog, catalog, catalog]));
  await chmod(binary, 0o700);
  fixtureLaunch.script = binary;
  const phases: string[] = [];
  await serve!.ensureServe(undefined, { report: (message) => phases.push(message) });
  expect(phases[0]).toBe("starting freellama serve");
  expect(phases).toContain("freellama serve: waiting for catalog (first fetch)");
  expect(phases.at(-1)).toBe("freellama serve ready");
  const { readFile } = await import("node:fs/promises");
  const args = JSON.parse(await readFile(argsFile, "utf8")) as string[];
  expect(args.slice(args.indexOf("--runtime-config"), args.indexOf("--runtime-config") + 2)).toEqual(["--runtime-config", join(home, "runtime.toml")]);
  expect(await serve!.readiness(endpoint, 1_000)).toEqual({ state: "ready" });
  await rm(home, { recursive: true, force: true });
}, 15_000);

it.skipIf(process.platform === "win32")("stops waiting when serve answers but Ollama is down, and omits a missing runtime file", async () => {
  const home = await mkdtemp(join(tmpdir(), "freellama-home-"));
  vi.stubEnv("FREELLAMA_HOME", home);
  vi.stubEnv("FREELLAMA_RUNTIME_CONFIG", "");
  const { binary } = await fixture();
  const argsFile = join(home, "args.json");
  const down = { status: 503, body: { ready: false, waiting_for: "ollama", detail: "connection refused" } };
  await writeFile(binary, readyScript(argsFile, Array.from({ length: 1_000 }, () => down)));
  await chmod(binary, 0o700);
  fixtureLaunch.script = binary;
  const started = Date.now();
  await serve!.ensureServe(undefined);
  expect(Date.now() - started).toBeLessThan(8_000);
  const { readFile } = await import("node:fs/promises");
  expect(JSON.parse(await readFile(argsFile, "utf8"))).not.toContain("--runtime-config");
  await rm(home, { recursive: true, force: true });
}, 15_000);

it.skipIf(process.platform === "win32")("ends a caller's wait on abort without stopping the start", async () => {
  const { binary, endpoint } = await fixture();
  // Binds only after a delay, as a slow startup would.
  await writeFile(binary, `#!${process.execPath}\nsetTimeout(() => import('node:http').then(({createServer}) => {
    const args = process.argv.slice(2); const listen = args[args.indexOf('--listen') + 1];
    createServer((req,res) => { res.end('{}'); }).listen(Number(listen.split(':').at(-1)), '127.0.0.1');
  }), 1_500);\n`);
  await chmod(binary, 0o700);
  fixtureLaunch.script = binary;
  const controller = new AbortController();
  const waiting = serve!.ensureServe(undefined, { signal: controller.signal });
  setTimeout(() => controller.abort(), 200);
  await expect(waiting).rejects.toThrow(/Cancelled while freellama serve was starting/);
  await expect(serve!.ensureServe(undefined)).resolves.toBeUndefined();
  expect((await fetch(`${endpoint}/_freellama/v1/health`)).status).toBe(200);
}, 15_000);
