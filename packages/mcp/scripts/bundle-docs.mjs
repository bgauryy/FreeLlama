// Package the repository's operator documentation for installed MCP clients.
// The repository docs/ directory remains the source of truth. This build step copies only
// Markdown and creates a compact index, so an MCP client can fetch one relevant resource rather
// than receiving every guide in its initialization context.
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const pkg = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const source = path.join(pkg, "..", "..", "docs");
const destination = path.join(pkg, "docs");

if (!existsSync(source)) {
  console.error(`bundle-docs: missing ${source} — packaged agent documentation would be incomplete`);
  process.exit(1);
}

rmSync(destination, { recursive: true, force: true });
mkdirSync(destination, { recursive: true });

const names = readdirSync(source, { withFileTypes: true })
  .filter((entry) => entry.isFile() && entry.name.endsWith(".md"))
  .map((entry) => entry.name)
  .sort();

for (const name of names) copyFileSync(path.join(source, name), path.join(destination, name));

const index = names.map((name) => {
  const title = readFileSync(path.join(source, name), "utf8")
    .split("\n")
    .find((line) => line.startsWith("# "))?.slice(2) ?? name;
  return `- [${title}](${name})`;
});
writeFileSync(
  path.join(destination, "INDEX.md"),
  "# FreeLlama packaged documentation\n\n" +
    "Read only the document relevant to the current operation. Repository `docs/` is the source of truth.\n\n" +
    index.join("\n") +
    "\n",
);
console.log(`bundle-docs: copied ${names.length} documents into docs/`);
