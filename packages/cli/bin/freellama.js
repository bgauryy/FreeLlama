#!/usr/bin/env node
// Launcher so `npx @octocodeai/freellama` works: hands the process to the compiled Rust binary, or,
// for the agent commands (run, chat, gateway…), to the agent CLI in Node.
//
// The binary is platform-specific and compiled by Cargo, so it is shipped in the tarball rather
// than built on install — an install-time `cargo build` would need a Rust toolchain on every
// consumer's machine. On a platform with no shipped binary the error names what it looked for and
// how to build one, instead of a bare ENOENT from deep inside a spawn.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? "freellama.exe" : "freellama";
const requireFromHere = createRequire(import.meta.url);
const ABI_SUFFIX = { linux: "-gnu", win32: "-msvc" }[process.platform] ?? "";
const platformId = `${process.platform}-${process.arch}${ABI_SUFFIX}`;
const candidates = [
  path.join(root, "..", "..", "target", "release", exe), // in-repo dev, no packing needed
];

try {
  candidates.push(path.join(path.dirname(requireFromHere.resolve(`@octocodeai/freellama-native-${platformId}/package.json`)), exe));
} catch {
  // Optional packages for other platforms are intentionally absent after install.
}

// `freellama run`, `chat`, `gateway`… run in Node: they reuse the MCP server's workflows, model
// picks, prompts and checks (packages/mcp/src/agent-cli), installed with this package as a
// dependency. On a terminal, `status` is their human view; piped or with --json it stays the
// binary's JSON receipt, so scripts keep working (and it falls back to the binary if the agent
// CLI is missing).
const AGENT_COMMANDS = new Set(["run", "chat", "gateway", "workflows", "job", "tool", "tools"]);
const args = process.argv.slice(2);
const agentCommand = AGENT_COMMANDS.has(args[0]);
const humanStatus = args[0] === "status" && process.stdout.isTTY === true && !args.includes("--json");
const agent = agentCommand || humanStatus ? agentCli() : undefined;

if (agent) {
  hand(process.execPath, [agent, ...args]);
} else if (agentCommand) {
  console.error(
    `freellama: \`${args[0]}\` runs in Node from @octocodeai/freellama-mcp-server, which was not found.\n` +
      "Reinstall @octocodeai/freellama (it depends on it), or from a checkout run `yarn build`.",
  );
  process.exit(1);
} else {
  const binary = candidates.find((p) => existsSync(p));
  if (!binary) {
    console.error(
      `freellama: no binary for ${process.platform}-${process.arch}.\n` +
        `Looked in:\n${candidates.map((c) => `  ${c}`).join("\n")}\n` +
        "Build it from a checkout with:\n  cargo build --release\n" +
        "Reinstall without --omit=optional. From a source checkout, run `cargo build --release`.",
    );
    process.exit(1);
  }
  // The binary's `status` always prints JSON and has no --json flag: the flag only keeps the
  // human view away, so it is not passed on.
  hand(binary, args[0] === "status" ? args.filter((arg) => arg !== "--json") : args);
}

/** The agent CLI: a checkout's build first, then the installed MCP package's. */
function agentCli() {
  const local = path.join(root, "..", "mcp", "dist", "agent-cli.js");
  if (existsSync(local)) return local;
  try {
    const file = path.join(path.dirname(requireFromHere.resolve("@octocodeai/freellama-mcp-server/package.json")), "dist", "agent-cli.js");
    return existsSync(file) ? file : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Run `file` with stdio passed through, forwarding termination signals and the exit code: this is
 * a launcher, not a wrapper that reinterprets output.
 */
function hand(file, argv) {
  const child = spawn(file, argv, { stdio: "inherit" });
  // Forward termination to the child. Without this, a supervisor (or `kill`) that signals the npx
  // process by PID kills only this launcher, and `freellama serve` is orphaned still holding its
  // listener — the next start then fails with "address already in use" and nothing explains why.
  // Ctrl-C already reaches both via the process group; this covers the targeted-signal case.
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, () => {
      if (!child.killed) child.kill(signal);
    });
  }
  // Reproduce the shell's own convention for a signalled child (128 + signal number) so callers can
  // tell "interrupted" from "failed"; both used to collapse into a bare 1.
  child.on("exit", (code, signal) => {
    if (signal) {
      process.exit(128 + (os.constants.signals[signal] ?? 15));
    }
    process.exit(code ?? 0);
  });
  child.on("error", (err) => {
    console.error(`freellama: failed to start ${file}: ${err.message}`);
    process.exit(1);
  });
}
