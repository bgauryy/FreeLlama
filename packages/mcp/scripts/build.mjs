// One MCP artifact: dist/index.js. Native .node stays next door (cannot go in a JS bundle);
// research adapters are copied after the JS build for packed installs.
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
await import("./bundle-adapters.mjs");
