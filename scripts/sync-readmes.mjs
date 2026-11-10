#!/usr/bin/env node
// The npm packages ship the root README. Their copies must link to GitHub, because the docs a
// relative link names are not in the tarball (and on npm there is no repository to resolve against).
// `node scripts/sync-readmes.mjs` writes the copies; `--check` exits 1 when one is stale.
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const BLOB = "https://github.com/bgauryy/FreeLlama/blob/main/";
const COPIES = ["packages/mcp/README.md", "packages/cli/README.md"];

/**
 * Root-relative links as GitHub URLs. Absolute and in-page links stay, and so do images: each
 * package ships its own copy (`assets/logo.jpg` is in its `files`).
 */
export function packageReadme(markdown) {
  return markdown.replace(/\]\((?!https?:|mailto:|#)([^)\s]+)\)/g, (match, target) =>
    /\.(png|jpe?g|gif|svg|webp)$/i.test(target) ? match : `](${BLOB}${target.replace(/^\.\//, "")})`);
}

const expected = packageReadme(readFileSync(path.join(root, "README.md"), "utf8"));
const check = process.argv.includes("--check");
let stale = 0;
for (const copy of COPIES) {
  const file = path.join(root, copy);
  if (check) {
    if (readFileSync(file, "utf8") !== expected) {
      console.error(`${copy} is stale; run: node scripts/sync-readmes.mjs`);
      stale += 1;
    }
  } else writeFileSync(file, expected);
}
process.exit(stale > 0 ? 1 : 0);
