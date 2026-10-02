import { build } from "esbuild";

await build({
  entryPoints: ["src/server/index.ts"],
  outfile: "dist/server/index.js",
  bundle: true,
  platform: "node",
  format: "esm",
  target: "node20",
  packages: "external",
  sourcemap: true,
});
