import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { fileURLToPath } from "node:url";

// Resolve the built server from this file, not the working directory. Every one of these
// scripts used to pass a bare "dist/index.js", so they only ran from packages/mcp — the
// command the README documents (`node packages/mcp/test/validate-all.mjs`, from the repo
// root) failed with MODULE_NOT_FOUND before the server ever started.
const SERVER_ENTRY = fileURLToPath(new URL("../dist/index.js", import.meta.url));

const transport = new StdioClientTransport({ command: "node", args: [SERVER_ENTRY] });
const client = new Client({ name: "test-client", version: "0.0.1" });
await client.connect(transport);

console.log("=== tools/call: delegate_research ===");
const result = await client.callTool({
  name: "delegate_research",
  arguments: {
    question:
      "In packages/rust-core/Cargo.toml, what is the name of the optional feature that enables the Node addon?",
    workspacePath: "/Users/guybary/Documents/code/FreeLlama",
  },
});
console.log("isError:", result.isError ?? false);
console.log(result.content[0].text);

await client.close();
process.exit(0);
