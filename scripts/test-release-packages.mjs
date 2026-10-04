import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { PLATFORM_PACKAGES, addonName, executableName } from "./release-platforms.mjs";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const heroImage = readFileSync(path.join(repo, "assets/logo.jpg"));
const untouchedBinary = `binary ${JSON.parse(readFileSync(path.join(repo, "package.json"), "utf8")).version}\n`;
const scratch = path.join(repo, ".octocode/tmp/prepublish/packaging");
mkdirSync(scratch, { recursive: true });
const adapters = ["agent_context.py", "agent_transport.py", "shell_sandbox.py", "bash_agent.py", "octocode_agent.py"];

function fixture() {
  const directory = mkdtempSync(path.join(scratch, "verify-"));
  const put = (name, content = "fixture\n") => {
    const destination = path.join(directory, name);
    mkdirSync(path.dirname(destination), { recursive: true });
    writeFileSync(destination, content);
  };
  put("package.json", readFileSync(path.join(repo, "package.json")));
  const version = JSON.parse(readFileSync(path.join(directory, "package.json"))).version;
  for (const pkg of ["cli", "mcp", ...PLATFORM_PACKAGES.map(({ id }) => `native/${id}`)]) {
    put(`packages/${pkg}/package.json`, readFileSync(path.join(repo, `packages/${pkg}/package.json`)));
    for (const file of ["LICENSE-APACHE", "LICENSE-MIT"]) put(`packages/${pkg}/${file}`, readFileSync(path.join(repo, file)));
    put(`packages/${pkg}/assets/logo.jpg`, heroImage);
  }
  for (const file of ["packages/cli/bin/freellama.js", "packages/mcp/dist/index.js", "packages/mcp/native/index.js", "packages/mcp/native/index.d.ts", "packages/mcp/native/package.json", ...adapters.map((name) => `packages/mcp/adapters/${name}`)]) put(file);
  for (const { id } of PLATFORM_PACKAGES) {
    put(`packages/native/${id}/${addonName(id)}`, `addon ${version}\n`);
    put(`packages/native/${id}/${executableName(id)}`, `binary ${version}\n`);
  }
  const toolDirectory = path.join(directory, "tools");
  put("tools/package.json", '{"type":"commonjs"}');
  put("tools/npm", `#!${process.execPath}\nconst fs = require('node:fs'); const path = require('node:path'); function walk(dir, base='') { return fs.readdirSync(dir, {withFileTypes:true}).flatMap(entry => entry.isDirectory() ? walk(path.join(dir,entry.name), path.join(base,entry.name)) : [{path:path.join(base,entry.name)}]); } process.stdout.write(JSON.stringify([{files:walk(process.cwd()).filter(file => file.path !== process.env.PACK_EXCLUDE)}]));\n`);
  put("tools/cargo", `#!${process.execPath}\nconst fs = require('node:fs'); process.stdout.write(fs.readFileSync(${JSON.stringify(path.join(directory, "cargo-metadata.json"))},'utf8'));\n`);
  chmodSync(path.join(toolDirectory, "npm"), 0o755);
  chmodSync(path.join(toolDirectory, "cargo"), 0o755);
  const metadata = { packages: ["freellama-core", "freellama-cli"].map((name) => ({ name, version, publish: [] })) };
  put("cargo-metadata.json", JSON.stringify(metadata));
  return {
    directory, put, metadata,
    run(env = {}) { return spawnSync(process.execPath, [path.join(repo, "scripts/verify-release-packages.mjs")], { cwd: directory, encoding: "utf8", env: { ...process.env, PATH: `${toolDirectory}${path.delimiter}${process.env.PATH}`, FREELLAMA_REQUIRE_ALL_PLATFORMS: "1", ...env } }); },
    cleanup() { rmSync(directory, { recursive: true, force: true }); },
  };
}

test("complete release package fixture passes", () => {
  const f = fixture();
  try { const result = f.run(); assert.equal(result.status, 0, result.stderr); } finally { f.cleanup(); }
});

for (const file of ["packages/cli/bin/freellama.js", "packages/mcp/dist/index.js", ...adapters.map((name) => `packages/mcp/adapters/${name}`)]) {
  test(`missing runtime file is refused: ${file}`, () => {
    const f = fixture();
    try {
      rmSync(path.join(f.directory, file));
      const result = f.run();
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, new RegExp(`${file.replaceAll(".", "\\.")}: required runtime file is missing, empty, or excluded from npm pack`));
    } finally { f.cleanup(); }
  });
}

test("empty MCP entry point is refused", () => {
  const f = fixture();
  try {
    f.put("packages/mcp/dist/index.js", "");
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /dist\/index\.js: required runtime file is missing, empty, or excluded from npm pack/);
  } finally { f.cleanup(); }
});

test("runtime file present on disk but excluded from tarball is refused", () => {
  const f = fixture();
  try {
    const result = f.run({ PACK_EXCLUDE: "dist/index.js" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /dist\/index\.js: required runtime file is missing, empty, or excluded from npm pack/);
  } finally { f.cleanup(); }
});

test("native artifact built at another version is refused", () => {
  const f = fixture();
  try {
    f.put("packages/native/linux-x64-gnu/freellama", "binary 9.8.7\n");
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /linux-x64-gnu\/freellama: does not embed version/);
  } finally { f.cleanup(); }
});

test("Rust artifact version must match npm release version", () => {
  const f = fixture();
  try {
    f.metadata.packages[0].version = "9.8.7";
    f.put("cargo-metadata.json", JSON.stringify(f.metadata));
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /freellama-core: version 9\.8\.7 must match workspace version/);
  } finally { f.cleanup(); }
});

function assemblyFixture() {
  const f = fixture();
  for (const script of ["assemble-release.mjs", "release-platforms.mjs"]) {
    f.put(`scripts/${script}`, readFileSync(path.join(repo, "scripts", script)));
  }
  for (const { id } of PLATFORM_PACKAGES) {
    f.put(`release-artifacts/${id}/${addonName(id)}`, `addon ${id}\n`);
    f.put(`release-artifacts/${id}/${executableName(id)}`, `binary ${id}\n`);
  }
  f.put("release/previous-release", "preserve until inputs validated\n");
  f.run = (args = []) => spawnSync(process.execPath, [path.join(f.directory, "scripts/assemble-release.mjs"), ...args], { cwd: f.directory, encoding: "utf8" });
  return f;
}

for (const output of [".", "release-artifacts", "packages/native", "release-artifacts/darwin-arm64"]) {
  test(`assembly refuses destructive output ${output} before touching inputs or packages`, () => {
    const f = assemblyFixture();
    try {
      const result = f.run(["release-artifacts", output]);
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, /unsafe release output/);
      assert.equal(readFileSync(path.join(f.directory, "release/previous-release"), "utf8"), "preserve until inputs validated\n");
      assert.equal(readFileSync(path.join(f.directory, "release-artifacts/darwin-arm64/freellama"), "utf8"), "binary darwin-arm64\n");
      assert.equal(readFileSync(path.join(f.directory, "packages/native/darwin-arm64/freellama"), "utf8"), untouchedBinary);
    } finally { f.cleanup(); }
  });
}

test("assembly permits a separate custom output directory", () => {
  const f = assemblyFixture();
  try {
    const result = f.run(["release-artifacts", "custom-output"]);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(path.join(f.directory, "custom-output/freellama-darwin-arm64"), "utf8"), "binary darwin-arm64\n");
    assert.equal(readFileSync(path.join(f.directory, "release/previous-release"), "utf8"), "preserve until inputs validated\n");
  } finally { f.cleanup(); }
});

test("assembly refuses a late missing artifact before replacing output or platform packages", () => {
  const f = assemblyFixture();
  try {
    const id = PLATFORM_PACKAGES.at(-1).id;
    rmSync(path.join(f.directory, `release-artifacts/${id}/${executableName(id)}`));
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, new RegExp(`${id}: expected`));
    assert.equal(readFileSync(path.join(f.directory, "release/previous-release"), "utf8"), "preserve until inputs validated\n");
    assert.equal(readFileSync(path.join(f.directory, "packages/native/darwin-arm64/freellama"), "utf8"), untouchedBinary);
  } finally { f.cleanup(); }
});

for (const scenario of ["duplicate", "empty"]) {
  test(`assembly refuses ${scenario} native artifacts`, () => {
    const f = assemblyFixture();
    try {
      const artifact = "release-artifacts/darwin-arm64/freellama";
      if (scenario === "duplicate") f.put("release-artifacts/darwin-arm64/nested/freellama");
      else f.put(artifact, "");
      const result = f.run();
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, scenario === "duplicate" ? /darwin-arm64: expected exactly one freellama, found 2/ : /darwin-arm64: freellama must be a nonempty regular file/);
    } finally { f.cleanup(); }
  });
}

test("assembly copies exact platform inputs and emits matching checksums", () => {
  const f = assemblyFixture();
  try {
    const result = f.run();
    assert.equal(result.status, 0, result.stderr);
    const lines = readFileSync(path.join(f.directory, "release/SHA256SUMS"), "utf8").trim().split("\n");
    assert.equal(lines.length, PLATFORM_PACKAGES.length);
    for (const { id, os } of PLATFORM_PACKAGES) {
      const name = `freellama-${id}${os === "win32" ? ".exe" : ""}`;
      const contents = readFileSync(path.join(f.directory, "release", name));
      assert.equal(contents.toString(), `binary ${id}\n`);
      assert.ok(lines.includes(`${createHash("sha256").update(contents).digest("hex")}  ${name}`));
      assert.equal(readFileSync(path.join(f.directory, `packages/native/${id}/${addonName(id)}`), "utf8"), `addon ${id}\n`);
      if (os !== "win32") {
        assert.equal(statSync(path.join(f.directory, "release", name)).mode & 0o777, 0o755);
        assert.equal(statSync(path.join(f.directory, `packages/native/${id}/${executableName(id)}`)).mode & 0o777, 0o755);
      }
    }
  } finally { f.cleanup(); }
});


test("missing agentic tool image rejects publication", () => {
  const f = fixture();
  try {
    rmSync(path.join(f.directory, "packages/cli/assets/logo.jpg"));
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /assets\/logo\.jpg: required runtime file is missing, empty, or excluded from npm pack/);
  } finally { f.cleanup(); }
});

test("agentic tool image excluded from the tarball rejects publication", () => {
  const f = fixture();
  try {
    const result = f.run({ PACK_EXCLUDE: "assets/logo.jpg" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /assets\/logo\.jpg: required runtime file is missing, empty, or excluded from npm pack/);
  } finally { f.cleanup(); }
});

test("missing package license text rejects publication", () => {
  const f = fixture();
  try {
    rmSync(path.join(f.directory, "packages/mcp/LICENSE-MIT"));
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /LICENSE-MIT.*missing, empty, or excluded/);
  } finally { f.cleanup(); }
});


test("release tag must match package version", () => {
  const f = fixture();
  try {
    const result = f.run({ FREELLAMA_RELEASE_TAG: "v9.8.7" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /release tag v9\.8\.7 must match workspace version/);
  } finally { f.cleanup(); }
});
