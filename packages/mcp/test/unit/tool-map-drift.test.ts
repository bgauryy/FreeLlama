// Guards against the one drift the CLI's hand-maintained tool table can suffer: the MCP server
// (packages/mcp/src/index.ts) is the source of truth for which tools exist, but the CLI prints its
// own copy of that list in `print_tool_map` (packages/cli/src/main.rs). Add, remove, or rename a
// tool on one side and forget the other, and the two silently disagree. This test parses the tool
// names out of both source files and asserts the sets are identical.
//
// Pure source parsing: no build, no Ollama, no network — safe to run in CI on every change.
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const here = path.dirname(fileURLToPath(import.meta.url));
const indexTs = path.join(here, "..", "..", "src", "index.ts");
const mainRs = path.join(here, "..", "..", "..", "cli", "src", "main.rs");

// MCP side: every `server.registerTool("<name>", ...)` call. A newline follows the `(`, so match
// across it rather than on a single line.
const registered = [...readFileSync(indexTs, "utf8").matchAll(/registerTool\(\s*"([a-z_]+)"/g)]
  .map((m) => m[1])
  .sort();

describe("CLI tool map vs MCP server", () => {
  it("finds a plausible number of registered MCP tools", () => {
    expect(registered.length).toBeGreaterThanOrEqual(6);
  });

  it("advertises the identical tool set in print_tool_map", () => {
    // CLI side: the first string of each 3-tuple in `print_tool_map`'s `rows` array. Each tuple
    // opens with `(` at 8 spaces of indent, and the tool name is the next line at 12 spaces.
    const rows = readFileSync(mainRs, "utf8").match(/fn print_tool_map[\s\S]*?\n\}/);
    expect(rows, "could not locate print_tool_map in packages/cli/src/main.rs").toBeTruthy();
    const advertised = [...rows![0].matchAll(/^ {8}\(\n {12}"([a-z_]+)",/gm)].map((m) => m[1]).sort();
    expect(advertised).toEqual(registered);
  });

  it("states the honest tool count in the CLI header prose", () => {
    const header = readFileSync(mainRs, "utf8").match(/exposes (\d+) MCP tools/);
    expect(header, "print_tool_map no longer states the tool count").toBeTruthy();
    expect(Number(header![1])).toBe(registered.length);
  });
});
