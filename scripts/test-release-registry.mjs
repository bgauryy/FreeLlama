import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { PLATFORM_PACKAGES, nativePackageName } from "./release-platforms.mjs";

const gate = fileURLToPath(new URL("./verify-release-registry.mjs", import.meta.url));
const version = "2.7.13";
const packages = [
  ["packages/cli", "@octocodeai/freellama"],
  ["packages/mcp", "@octocodeai/freellama-mcp-server"],
  ...PLATFORM_PACKAGES.map(({ id }) => [`packages/native/${id}`, nativePackageName(id)]),
];

async function fixture(reply) {
  const directory = mkdtempSync(path.join(tmpdir(), "freellama-registry-"));
  const put = (file, value) => {
    mkdirSync(path.dirname(path.join(directory, file)), { recursive: true });
    writeFileSync(path.join(directory, file), JSON.stringify(value));
  };
  put("package.json", { version });
  for (const [location, name] of packages) put(`${location}/package.json`, { name, version });
  const requests = [];
  const server = http.createServer((request, response) => {
    const name = decodeURIComponent(request.url.slice(1));
    requests.push({ name, method: request.method, accept: request.headers.accept });
    reply(name, response);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const registry = `http://127.0.0.1:${server.address().port}/`;
  return {
    put, requests,
    run(env = {}) {
      return new Promise((resolve, reject) => {
        const child = spawn(process.execPath, [gate], {
          cwd: directory,
          env: { ...process.env, FREELLAMA_NPM_REGISTRY: registry, FREELLAMA_RELEASE_TAG: `v${version}`, FREELLAMA_NPM_REGISTRY_TIMEOUT_MS: "1000", ...env },
          stdio: ["ignore", "pipe", "pipe"],
        });
        let stdout = "";
        let stderr = "";
        const timeout = setTimeout(() => child.kill("SIGKILL"), 10_000);
        child.stdout.on("data", (chunk) => { stdout += chunk; });
        child.stderr.on("data", (chunk) => { stderr += chunk; });
        child.once("error", (error) => { clearTimeout(timeout); reject(error); });
        child.once("close", (code, signal) => { clearTimeout(timeout); resolve({ code, signal, stdout, stderr }); });
      });
    },
    async cleanup() {
      server.closeAllConnections();
      await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
      rmSync(directory, { recursive: true, force: true });
    },
  };
}

function metadata(response, name, versions = {}) {
  response.writeHead(200, { "content-type": "application/json" });
  response.end(JSON.stringify({ name, versions }));
}

test("all missing release versions pass after checking the complete package matrix", async () => {
  const f = await fixture((name, response) => {
    if (name === packages[0][1]) { response.writeHead(404); response.end("absent"); }
    else metadata(response, name, { "1.0.0": { version: "1.0.0" } });
  });
  try {
    const result = await f.run();
    assert.equal(result.code, 0, result.stderr);
    assert.equal(f.requests.length, 10);
    assert.deepEqual(new Set(f.requests.map(({ name }) => name)), new Set(packages.map(([, name]) => name)));
    assert.ok(f.requests.every(({ method, accept }) => method === "GET" && accept === "application/json"));
  } finally { await f.cleanup(); }
});

for (const [label, collision] of [["CLI", packages[0][1]], ["last native platform", packages.at(-1)[1]]]) {
  test(`${label} existing version blocks the release after all package checks`, async () => {
    const f = await fixture((name, response) => metadata(response, name, name === collision ? { [version]: { version } } : {}));
    try {
      const result = await f.run();
      assert.notEqual(result.code, 0);
      assert.match(result.stderr, /already published/);
      assert.ok(result.stderr.includes(`${collision}@${version}`), result.stderr);
      assert.equal(f.requests.length, 10);
    } finally { await f.cleanup(); }
  });
}

for (const status of [401, 403, 500]) {
  test(`HTTP ${status} fails closed`, async () => {
    const f = await fixture((name, response) => {
      if (name === packages.at(-1)[1]) { response.writeHead(status); response.end("registry unavailable"); }
      else metadata(response, name);
    });
    try {
      const result = await f.run();
      assert.notEqual(result.code, 0);
      assert.match(result.stderr, new RegExp(`HTTP ${status}`));
      assert.equal(f.requests.length, 10);
    } finally { await f.cleanup(); }
  });
}

for (const body of ["not JSON", "null", '[]', '{"versions":{}}', '{"name":"wrong","versions":{}}', '{"name":"@octocodeai/freellama","versions":[]}', '{"name":"@octocodeai/freellama","versions":{"1.0.0":null}}', '{"name":"@octocodeai/freellama","versions":{"1.0.0":{"version":"1.0.1"}}}']) {
  test(`malformed metadata fails closed: ${body}`, async () => {
    const f = await fixture((name, response) => {
      if (name === packages[0][1]) { response.writeHead(200); response.end(body); }
      else metadata(response, name);
    });
    try {
      const result = await f.run();
      assert.notEqual(result.code, 0);
      assert.match(result.stderr, /invalid registry metadata/);
    } finally { await f.cleanup(); }
  });
}

for (const headersSent of [false, true]) {
  test(`hung registry ${headersSent ? "body" : "response"} is bounded and fails closed`, async () => {
    const f = await fixture((name, response) => {
      if (name !== packages[0][1]) metadata(response, name);
      else if (headersSent) { response.writeHead(200); response.write('{"name":'); }
    });
    try {
      const result = await f.run({ FREELLAMA_NPM_REGISTRY_TIMEOUT_MS: "100" });
      assert.notEqual(result.code, 0);
      assert.match(result.stderr, /registry request failed/);
      assert.equal(result.signal, null);
    } finally { await f.cleanup(); }
  });
}

test("a dropped registry connection fails closed", async () => {
  const f = await fixture((name, response) => {
    if (name === packages[0][1]) response.destroy();
    else metadata(response, name);
  });
  try {
    const result = await f.run();
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /registry request failed/);
  } finally { await f.cleanup(); }
});

test("manifest and release-tag mismatches fail before registry requests", async () => {
  const f = await fixture((name, response) => metadata(response, name));
  try {
    f.put(`${packages.at(-1)[0]}/package.json`, { name: packages.at(-1)[1], version: "2.7.14" });
    const mismatch = await f.run();
    assert.notEqual(mismatch.code, 0);
    assert.match(mismatch.stderr, /must match workspace version 2\.7\.13/);
    f.put(`${packages.at(-1)[0]}/package.json`, { name: packages.at(-1)[1], version });
    const tag = await f.run({ FREELLAMA_RELEASE_TAG: "v2.7.14" });
    assert.notEqual(tag.code, 0);
    assert.match(tag.stderr, /release tag v2\.7\.14 must match workspace version v2\.7\.13/);
    assert.equal(f.requests.length, 0);
  } finally { await f.cleanup(); }
});
