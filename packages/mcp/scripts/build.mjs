// MCP artifacts: dist/index.js (the server) and dist/research/{bash,octocode}-agent.js (the
// research adapters it spawns). Native .node stays next door (cannot go in a JS bundle).
import { build } from "esbuild";
import { chmodSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const pkg = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const distDir = path.join(pkg, "dist");
const outfile = path.join(distDir, "index.js");

rmSync(distDir, { recursive: true, force: true });

await build({
  absWorkingDir: pkg,
  entryPoints: [path.join(pkg, "src", "index.ts")],
  outfile,
  bundle: true,
  format: "esm",
  platform: "node",
  target: "node20",
  sourcemap: false,
  logLevel: "info",
  external: ["../native/index.js"],
});

const js = readFileSync(outfile, "utf8").replace(/^(?:#!.*\n)+/, "");
writeFileSync(outfile, `#!/usr/bin/env node\n${js}`);
chmodSync(outfile, 0o755);

// Each research adapter is a standalone `node <file>` program with its own bundle.
await build({
  absWorkingDir: pkg,
  entryPoints: {
    "bash-agent": path.join(pkg, "src", "research", "bash-agent.ts"),
    "octocode-agent": path.join(pkg, "src", "research", "octocode-agent.ts"),
  },
  outdir: path.join(distDir, "research"),
  bundle: true,
  format: "esm",
  platform: "node",
  target: "node20",
  sourcemap: false,
  logLevel: "info",
});
await import("./bundle-docs.mjs");

await import("../../../scripts/bundle-licenses.mjs");
