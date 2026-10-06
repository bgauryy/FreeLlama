// Packaged documentation as MCP resources (freellama://docs/<NAME>).
import { existsSync, readdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

// Documentation is bundled at build time from the repository docs/ directory. Resources keep it
// out of the always-present tool instruction budget while giving MCP clients an on-demand,
// package-local operating manual after npm installation. docs/README.md is the index.
export function registerDocsResources(server: McpServer): void {
  const PACKAGED_DOCS_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "docs");
  if (!existsSync(path.join(PACKAGED_DOCS_DIR, "README.md"))) {
    throw new Error(`FreeLlama MCP documentation index is missing at ${PACKAGED_DOCS_DIR}/README.md; run the package build.`);
  }
  const packagedDocs = readdirSync(PACKAGED_DOCS_DIR).filter((name) => name.endsWith(".md")).sort();
  for (const name of packagedDocs) {
    const isIndex = name === "README.md";
    const slug = isIndex ? "index" : name.replace(/\.md$/, "");
    const uri = `freellama://docs/${slug}`;
    server.registerResource(
      `freellama-docs-${slug.toLowerCase()}`,
      uri,
      {
        mimeType: "text/markdown",
        description: isIndex
          ? "Index of packaged FreeLlama operator and agent documentation. Read this first, then fetch one relevant document."
          : `Packaged FreeLlama documentation: ${name}.`,
      },
      async () => ({
        contents: [{ uri, mimeType: "text/markdown", text: await readFile(path.join(PACKAGED_DOCS_DIR, name), "utf8") }],
      }),
    );
  }
}
