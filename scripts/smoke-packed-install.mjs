#!/usr/bin/env node
// Packed-install smoke test: installs the tarballs of packages/cli, packages/mcp and this host's
// packages/native/<id> into a fresh project and checks what a user would run. In-repo launchers
// prefer target/release and the dev addon, so only this test sees a stale shipped artifact.
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { PLATFORM_PACKAGES, nativePackageName } from "./release-platforms.mjs";

const root = path.resolve(import.meta.dirname, "..");
const version = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8")).version;
const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const shell = process.platform === "win32";

function hostTarget() {
  const candidates = PLATFORM_PACKAGES.filter((t) => t.os === process.platform && t.cpu === process.arch);
  if (process.platform === "linux") {
    const glibc = Boolean(process.report?.getReport()?.header?.glibcVersionRuntime);
    return candidates.find((t) => (t.libc === "musl") === !glibc) ?? candidates[0];
  }
  return candidates[0];
}

const target = hostTarget();
if (!target) throw new Error(`no release package for ${process.platform}-${process.arch}`);
const failures = [];
const check = (name, ok, detail) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${ok ? "" : `: ${detail}`}`);
  if (!ok) failures.push(`${name}: ${detail}`);
};

const temp = mkdtempSync(path.join(os.tmpdir(), "freellama-smoke-"));
try {
  const tarballs = path.join(temp, "tarballs");
  const project = path.join(temp, "project");
  mkdirSync(tarballs);
  mkdirSync(project);
  const packed = ["packages/cli", "packages/mcp", `packages/native/${target.id}`].map((directory) => {
    const report = JSON.parse(execFileSync(npm, ["pack", "--json", "--ignore-scripts", "--pack-destination", tarballs], {
      cwd: path.join(root, directory), encoding: "utf8", shell,
    }))[0];
    return path.join(tarballs, report.filename);
  });
  writeFileSync(path.join(project, "package.json"), JSON.stringify({ name: "freellama-smoke", private: true, version: "0.0.0" }));
  // The tarballs are installed directly; the other platforms' optional packages and the MCP
  // dependencies resolve from the registry (an unpublished optional package is skipped).
  execFileSync(npm, ["install", "--no-audit", "--no-fund", "--ignore-scripts", ...packed], { cwd: project, stdio: "inherit", shell });

  const bin = (name) => path.join(project, "node_modules", ".bin", process.platform === "win32" ? `${name}.cmd` : name);
  const run = (file, args) => execFileSync(file, args, { cwd: project, encoding: "utf8", shell, env: { ...process.env, PATH: process.env.PATH } });

  // 1. Version of the installed launcher + executable.
  let installedVersion = "";
  try { installedVersion = run(bin("freellama"), ["--version"]).split("\n")[0].trim(); } catch (error) { installedVersion = String(error.message); }
  check("freellama --version", installedVersion === `freellama ${version}`, `got ${JSON.stringify(installedVersion)}, expected "freellama ${version}"`);

  // 2. Subcommands: the packaged executable must offer everything the workspace build does.
  const commands = (help) => {
    const lines = help.split("\n");
    const start = lines.findIndex((line) => /^Commands:/.test(line));
    const names = [];
    for (const line of lines.slice(start + 1)) {
      if (!line.trim()) break;
      const match = /^ {2}([a-z][\w-]*)\s/.exec(line + " ");
      if (match && match[1] !== "help") names.push(match[1]);
    }
    return names;
  };
  const reference = path.join(root, "target", "release", process.platform === "win32" ? "freellama.exe" : "freellama");
  const expected = commands(execFileSync(reference, ["--help"], { encoding: "utf8" }));
  let installed = [];
  try { installed = commands(run(bin("freellama"), ["--help"])); } catch (error) { installed = []; }
  const missing = expected.filter((name) => !installed.includes(name));
  check("freellama --help lists every subcommand of target/release/freellama", expected.length > 0 && missing.length === 0, expected.length ? `packaged executable lacks: ${missing.join(", ")}` : "reference executable lists no subcommands (run cargo build --release)");

  // 3. The installed addon loads and exports exactly what index.d.ts declares.
  const declared = [...readFileSync(path.join(root, "packages/mcp/native/index.d.ts"), "utf8").matchAll(/^export declare function (\w+)/gm)].map((m) => m[1]).sort();
  const projectRequire = createRequire(path.join(project, "package.json"));
  let exported = [];
  try { exported = Object.keys(projectRequire(nativePackageName(target.id))).sort(); } catch (error) { check("addon loads", false, error.message); }
  const extra = exported.filter((name) => !declared.includes(name));
  const absent = declared.filter((name) => !exported.includes(name));
  check("addon exports match packages/mcp/native/index.d.ts", exported.length > 0 && !extra.length && !absent.length, `extra in addon: [${extra.join(", ")}]; declared but missing: [${absent.join(", ")}]`);

  // 4. The installed MCP server starts over stdio and answers tools/list.
  const server = spawn(process.execPath, [path.join(project, "node_modules", "@octocodeai", "freellama-mcp-server", "dist", "index.js")], { cwd: project, stdio: ["pipe", "pipe", "pipe"] });
  const replies = new Map();
  let buffer = "";
  let stderr = "";
  server.stderr.on("data", (chunk) => { stderr += chunk; });
  server.stdout.on("data", (chunk) => {
    buffer += chunk;
    for (let newline = buffer.indexOf("\n"); newline >= 0; newline = buffer.indexOf("\n")) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (line) try { const message = JSON.parse(line); replies.set(message.id, message); } catch { /* non-JSON noise */ }
    }
  });
  const exchange = async (id, method, params) => {
    server.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    const deadline = Date.now() + 30_000;
    while (!replies.has(id)) {
      if (Date.now() > deadline || server.exitCode !== null) throw new Error(`no reply to ${method}; stderr: ${stderr.slice(-400)}`);
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return replies.get(id);
  };
  try {
    await exchange(1, "initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "smoke", version: "0" } });
    server.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
    const list = await exchange(2, "tools/list", {});
    const tools = list.result?.tools ?? [];
    check("MCP server answers tools/list", tools.length > 0, `result: ${JSON.stringify(list).slice(0, 300)}`);
  } catch (error) {
    check("MCP server answers tools/list", false, error.message);
  } finally {
    server.kill();
  }
} finally {
  rmSync(temp, { recursive: true, force: true });
}

if (failures.length) {
  console.error(`\npacked-install smoke FAILED (${failures.length}); rebuild the artifacts: yarn release:build -> node scripts/assemble-release.mjs release-artifacts release`);
  process.exit(1);
}
console.log("\npacked-install smoke passed");
