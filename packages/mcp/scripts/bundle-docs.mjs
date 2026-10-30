// Package the repository's operator documentation for installed MCP clients.
// The repository docs/ directory remains the source of truth. This build step copies only the
// top-level Markdown guides; docs/README.md is served as the index (freellama://docs/index), so an
// MCP client can fetch one relevant resource rather than receiving every guide at initialization.
import { copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const pkg = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const source = path.join(pkg, "..", "..", "docs");
const destination = path.join(pkg, "docs");

if (!existsSync(path.join(source, "README.md"))) {
  console.error(`bundle-docs: missing ${source}/README.md — packaged agent documentation would be incomplete`);
  process.exit(1);
}

rmSync(destination, { recursive: true, force: true });
mkdirSync(destination, { recursive: true });

const names = readdirSync(source, { withFileTypes: true })
  .filter((entry) => entry.isFile() && entry.name.endsWith(".md"))
  .map((entry) => entry.name)
  .sort();
for (const name of names) copyFileSync(path.join(source, name), path.join(destination, name));
console.log(`bundle-docs: copied ${names.length} documents into docs/`);
